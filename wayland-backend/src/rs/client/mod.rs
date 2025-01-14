//! Client-side rust implementation of a Wayland protocol backend

use std::{
    fmt,
    os::unix::{
        io::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
        net::UnixStream,
    },
    sync::{Arc, Condvar, Mutex},
};

use crate::{
    core_interfaces::WL_DISPLAY_INTERFACE,
    protocol::{
        check_for_signature, same_interface, same_interface_or_anonymous, AllowNull, Argument,
        ArgumentType, Interface, Message, ObjectInfo, ProtocolError, ANONYMOUS_INTERFACE,
        INLINE_ARGS,
    },
};
use smallvec::SmallVec;

use super::{
    debug::DisplaySlice,
    map::{Object, ObjectMap, SERVER_ID_LIMIT},
    socket::{BufferedSocket, Socket},
    wire::MessageParseError,
};

pub use crate::types::client::{InvalidId, NoWaylandLib, WaylandError};

/// A trait representing your data associated to an object
///
/// You will only be given access to it as a `&` reference, so you
/// need to handle interior mutability by yourself.
///
/// The methods of this trait will be invoked internally every time a
/// new object is created to initialize its data.
pub trait ObjectData: downcast_rs::DowncastSync {
    /// Dispatch an event for the associated object
    ///
    /// If the event has a NewId argument, the callback must return the object data
    /// for the newly created object
    fn event(
        self: Arc<Self>,
        handle: &mut Handle,
        msg: Message<ObjectId>,
    ) -> Option<Arc<dyn ObjectData>>;
    /// Notification that the object has been destroyed and is no longer active
    fn destroyed(&self, object_id: ObjectId);
    /// Helper for forwarding a Debug implementation of your `ObjectData` type
    ///
    /// By default will just print `ObjectData { ... }`
    fn debug(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectData").finish_non_exhaustive()
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for dyn ObjectData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.debug(f)
    }
}

downcast_rs::impl_downcast!(sync ObjectData);

#[derive(Debug, Clone)]
struct Data {
    client_destroyed: bool,
    server_destroyed: bool,
    user_data: Arc<dyn ObjectData>,
    serial: u32,
}

/// An ID representing a Wayland object
#[derive(Clone)]
pub struct ObjectId {
    serial: u32,
    id: u32,
    interface: &'static Interface,
}

impl std::cmp::PartialEq for ObjectId {
    fn eq(&self, other: &ObjectId) -> bool {
        self.id == other.id
            && self.serial == other.serial
            && same_interface(self.interface, other.interface)
    }
}

impl std::cmp::Eq for ObjectId {}

#[cfg(not(tarpaulin_include))]
impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.interface.name, self.id)
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({}, {})", self, self.serial)
    }
}

impl ObjectId {
    /// Check if this is the null ID
    pub fn is_null(&self) -> bool {
        self.id == 0
    }

    /// Interface of the represented object
    pub fn interface(&self) -> &'static Interface {
        self.interface
    }

    /// Return the protocol-level numerical ID of this object
    ///
    /// Protocol IDs are reused after object destruction, so this should not be used as a
    /// unique identifier,
    pub fn protocol_id(&self) -> u32 {
        self.id
    }
}

/// Main handle of a backend to the Wayland protocol
///
/// This type hosts most of the protocol-related functionality of the backend, and is the
/// main entry point for manipulating Wayland objects. It can be retrieved both from
/// the backend via [`Backend::handle()`](Backend::handle), and is given to you as argument
/// in most event callbacks.
#[derive(Debug)]
pub struct Handle {
    socket: BufferedSocket,
    map: ObjectMap<Data>,
    last_error: Option<WaylandError>,
    last_serial: u32,
    pending_placeholder: Option<(&'static Interface, u32)>,
    debug: bool,
}

/// A pure rust implementation of a Wayland client backend
///
/// This type hosts the plumbing functionalities for interacting with the wayland protocol,
/// and most of the protocol-level interactions are made through the [`Handle`] type, accessed
/// via the [`handle()`](Backend::handle) method.
#[derive(Debug)]
pub struct Backend {
    handle: Handle,
    prepared_reads: usize,
    read_condvar: Arc<Condvar>,
    read_serial: usize,
}

impl Backend {
    /// Try to initialize a Wayland backend on the provided unix stream
    ///
    /// The provided stream should correspond to an already established unix connection with
    /// the Wayland server. On this rust backend, this method never fails.
    pub fn connect(stream: UnixStream) -> Result<Self, NoWaylandLib> {
        let socket = BufferedSocket::new(unsafe { Socket::from_raw_fd(stream.into_raw_fd()) });
        let mut map = ObjectMap::new();
        map.insert_at(
            1,
            Object {
                interface: &WL_DISPLAY_INTERFACE,
                version: 1,
                data: Data {
                    client_destroyed: false,
                    server_destroyed: false,
                    user_data: Arc::new(DumbObjectData),
                    serial: 0,
                },
            },
        )
        .unwrap();

        let debug =
            matches!(std::env::var_os("WAYLAND_DEBUG"), Some(str) if str == "1" || str == "client");

        Ok(Backend {
            handle: Handle {
                socket,
                map,
                last_error: None,
                last_serial: 0,
                pending_placeholder: None,
                debug,
            },
            prepared_reads: 0,
            read_condvar: Arc::new(Condvar::new()),
            read_serial: 0,
        })
    }

    /// Flush all pending outgoing requests to the server
    pub fn flush(&mut self) -> Result<(), WaylandError> {
        self.handle.no_last_error()?;
        if let Err(e) = self.handle.socket.flush() {
            return Err(self.handle.store_if_not_wouldblock_and_return_error(e));
        }
        Ok(())
    }

    /// Read events from the wayland socket if available, and invoke the associated callbacks
    ///
    /// This function will never block, and returns an I/O `WouldBlock` error if no event is available
    /// to read.
    ///
    /// **Note:** this function should only be used if you know that you are the only thread
    /// reading events from the wayland socket. If this may not be the case, see [`ReadEventsGuard`]
    pub fn dispatch_events(&mut self) -> Result<usize, WaylandError> {
        self.handle.no_last_error()?;
        let mut dispatched = 0;
        loop {
            // Attempt to read a message
            let map = &self.handle.map;
            let message = match self.handle.socket.read_one_message(|id, opcode| {
                map.find(id)
                    .and_then(|o| o.interface.events.get(opcode as usize))
                    .map(|desc| desc.signature)
            }) {
                Ok(msg) => msg,
                Err(MessageParseError::MissingData) | Err(MessageParseError::MissingFD) => {
                    // need to read more data
                    if let Err(e) = self.handle.socket.fill_incoming_buffers() {
                        if e.kind() != std::io::ErrorKind::WouldBlock {
                            return Err(self.handle.store_and_return_error(e));
                        } else if dispatched == 0 {
                            return Err(e.into());
                        } else {
                            break;
                        }
                    }
                    continue;
                }
                Err(MessageParseError::Malformed) => {
                    // malformed error, protocol error
                    let err = WaylandError::Protocol(ProtocolError {
                        code: 0,
                        object_id: 0,
                        object_interface: "".into(),
                        message: "Malformed Wayland message.".into(),
                    });
                    return Err(self.handle.store_and_return_error(err));
                }
            };

            // We got a message, retrieve its associated object & details
            // These lookups must succeed otherwise we would not have been able to parse this message
            let receiver = self.handle.map.find(message.sender_id).unwrap();
            let message_desc = receiver.interface.events.get(message.opcode as usize).unwrap();

            // Short-circuit display-associated events
            if message.sender_id == 1 {
                self.handle.handle_display_event(message)?;
                continue;
            }

            let mut created_id = None;

            // Convert the arguments and create the new object if applicable
            let mut args = SmallVec::with_capacity(message.args.len());
            let mut arg_interfaces = message_desc.arg_interfaces.iter();
            for arg in message.args.into_iter() {
                args.push(match arg {
                    Argument::Array(a) => Argument::Array(a),
                    Argument::Int(i) => Argument::Int(i),
                    Argument::Uint(u) => Argument::Uint(u),
                    Argument::Str(s) => Argument::Str(s),
                    Argument::Fixed(f) => Argument::Fixed(f),
                    Argument::Fd(f) => Argument::Fd(f),
                    Argument::Object(o) => {
                        if o != 0 {
                            // Lookup the object to make the appropriate Id
                            let obj = match self.handle.map.find(o) {
                                Some(o) => o,
                                None => {
                                    let err = WaylandError::Protocol(ProtocolError {
                                        code: 0,
                                        object_id: 0,
                                        object_interface: "".into(),
                                        message: format!("Unknown object {}.", o),
                                    });
                                    return Err(self.handle.store_and_return_error(err));
                                }
                            };
                            if let Some(next_interface) = arg_interfaces.next() {
                                if !same_interface_or_anonymous(next_interface, obj.interface) {
                                    let err = WaylandError::Protocol(ProtocolError {
                                        code: 0,
                                        object_id: 0,
                                        object_interface: "".into(),
                                        message: format!(
                                            "Protocol error: server sent object {} for interface {}, but it has interface {}.",
                                            o, next_interface.name, obj.interface.name
                                        ),
                                    });
                                    return Err(self.handle.store_and_return_error(err));
                                }
                            }
                            Argument::Object(ObjectId { id: o, serial: obj.data.serial, interface: obj.interface })
                        } else {
                            Argument::Object(ObjectId { id: 0, serial: 0, interface: &ANONYMOUS_INTERFACE })
                        }
                    }
                    Argument::NewId(new_id) => {
                        // An object should be created
                        let child_interface = match message_desc.child_interface {
                            Some(iface) => iface,
                            None => panic!("Received event {}@{}.{} which creates an object without specifying its interface, this is unsupported.", receiver.interface.name, message.sender_id, message_desc.name),
                        };

                        let child_udata = Arc::new(UninitObjectData);

                        // if this ID belonged to a now destroyed server object, we can replace it
                        if new_id >= SERVER_ID_LIMIT
                            && self.handle.map.with(new_id, |obj| obj.data.client_destroyed).unwrap_or(false)
                        {
                            self.handle.map.remove(new_id);
                        }

                        let child_obj = Object {
                            interface: child_interface,
                            version: receiver.version,
                            data: Data {
                                client_destroyed: receiver.data.client_destroyed,
                                server_destroyed: false,
                                user_data: child_udata,
                                serial: self.handle.next_serial(),
                            }
                        };

                        let child_id = ObjectId { id: new_id, serial: child_obj.data.serial, interface: child_obj.interface };
                        created_id = Some(child_id.clone());

                        if let Err(()) = self.handle.map.insert_at(new_id, child_obj) {
                            // abort parsing, this is an unrecoverable error
                            let err = WaylandError::Protocol(ProtocolError {
                                code: 0,
                                object_id: 0,
                                object_interface: "".into(),
                                message: format!(
                                    "Protocol error: server tried to create \
                                    an object \"{}\" with invalid id {}.",
                                    child_interface.name, new_id
                                ),
                            });
                            return Err(self.handle.store_and_return_error(err));
                        }

                        Argument::NewId(child_id)
                    }
                });
            }

            if self.handle.debug {
                super::debug::print_dispatched_message(
                    receiver.interface.name,
                    message.sender_id,
                    message_desc.name,
                    &args,
                );
            }

            // If this event is send to an already destroyed object (by the client), swallow it
            if receiver.data.client_destroyed {
                // but close any associated FD to avoid leaking them
                for a in args {
                    if let Argument::Fd(fd) = a {
                        let _ = ::nix::unistd::close(fd);
                    }
                }
                continue;
            }

            // Invoke the user callback
            let id = ObjectId {
                id: message.sender_id,
                serial: receiver.data.serial,
                interface: receiver.interface,
            };
            log::debug!("Dispatching {}.{} ({})", id, receiver.version, DisplaySlice(&args));
            let ret = receiver
                .data
                .user_data
                .clone()
                .event(&mut self.handle, Message { sender_id: id, opcode: message.opcode, args });

            // If this event is a destructor, destroy the object
            if message_desc.is_destructor {
                self.handle
                    .map
                    .with(message.sender_id, |obj| {
                        obj.data.server_destroyed = true;
                        obj.data.client_destroyed = true;
                    })
                    .unwrap();
                receiver.data.user_data.destroyed(ObjectId {
                    id: message.sender_id,
                    serial: receiver.data.serial,
                    interface: receiver.interface,
                });
            }

            match (created_id, ret) {
                (Some(child_id), Some(child_data)) => {
                    self.handle
                        .map
                        .with(child_id.id, |obj| obj.data.user_data = child_data)
                        .unwrap();
                }
                (None, None) => {}
                (Some(child_id), None) => {
                    panic!(
                        "Callback creating object {} did not provide any object data.",
                        child_id
                    );
                }
                (None, Some(_)) => {
                    panic!("An object data was returned from a callback not creating any object");
                }
            }

            dispatched += 1;
        }
        Ok(dispatched)
    }

    /// Access the [`Handle`] associated with this backend
    pub fn handle(&mut self) -> &mut Handle {
        &mut self.handle
    }
}

/// Guard for synchronizing event reading across multiple threads
///
/// If multiple threads need to read events from the Wayland socket concurrently,
/// it is necessary to synchronize their access. Failing to do so may cause some of the
/// threads to not be notified of new events, and sleep much longer than appropriate.
///
/// To correctly synchronize access, this type should be used. The guard is created using
/// the [`try_new()`](ReadEventsGuard::try_new) method. And the event reading is triggered by consuming
/// the guard using the [`read()`](ReadEventsGuard::read) method.
///
/// If you plan to poll the Wayland socket for readiness, the file descriptor can be retrieved via
/// the [`connection_fd`](ReadEventsGuard::connection_fd) method. Note that for the synchronization to
/// correctly occur, you must *always* create the `ReadEventsGuard` *before* polling the socket.
#[derive(Debug)]
pub struct ReadEventsGuard {
    backend: Arc<Mutex<Backend>>,
    done: bool,
}

impl ReadEventsGuard {
    /// Create a new reading guard
    ///
    /// This call will not block, but event callbacks may be invoked in the process
    /// of preparing the guard.
    pub fn try_new(backend: Arc<Mutex<Backend>>) -> Result<Self, WaylandError> {
        backend.lock().unwrap().prepared_reads += 1;
        Ok(ReadEventsGuard { backend, done: false })
    }

    /// Access the Wayland socket FD for polling
    pub fn connection_fd(&self) -> RawFd {
        self.backend.lock().unwrap().handle.socket.as_raw_fd()
    }

    /// Attempt to read events from the Wayland socket
    ///
    /// If multiple threads have a live reading guard, this method will block until all of them
    /// are either dropped or have their `read()` method invoked, at which point on of the threads
    /// will read events from the socket and invoke the callbacks for the received events. All
    /// threads will then resume their execution.
    ///
    /// This returns the number of dispatched events, or `0` if an other thread handled the dispatching.
    /// If no events are available to read from the socket, this returns a `WouldBlock` IO error.
    pub fn read(mut self) -> Result<usize, WaylandError> {
        let mut backend = self.backend.lock().unwrap();
        backend.prepared_reads -= 1;
        self.done = true;
        if backend.prepared_reads == 0 {
            // We should be the one reading
            let ret = backend.dispatch_events();
            // wake up other threads
            backend.read_serial = backend.read_serial.wrapping_add(1);
            backend.read_condvar.notify_all();
            // forward the return value
            ret
        } else {
            // We should wait for an other thread to read (or cancel)
            let serial = backend.read_serial;
            let condvar = backend.read_condvar.clone();
            backend = condvar.wait_while(backend, |backend| serial == backend.read_serial).unwrap();
            backend.handle.no_last_error()?;
            Ok(0)
        }
    }
}

impl Drop for ReadEventsGuard {
    fn drop(&mut self) {
        if !self.done {
            let mut backend = self.backend.lock().unwrap();
            backend.prepared_reads -= 1;
            if backend.prepared_reads == 0 {
                // Cancel the read
                backend.read_serial = backend.read_serial.wrapping_add(1);
                backend.read_condvar.notify_all();
            }
        }
    }
}

impl Handle {
    /// Get the object ID for the `wl_display`
    pub fn display_id(&self) -> ObjectId {
        ObjectId { serial: 0, id: 1, interface: &WL_DISPLAY_INTERFACE }
    }

    /// Get the last error that occurred on this backend
    ///
    /// If this returns an error, your Wayland connection is already dead.
    pub fn last_error(&self) -> Option<WaylandError> {
        self.last_error.clone()
    }

    /// Get the detailed information about a wayland object
    ///
    /// Returns an error if the provided object ID is no longer valid.
    pub fn info(&self, id: ObjectId) -> Result<ObjectInfo, InvalidId> {
        let object = self.get_object(id.clone())?;
        if object.data.client_destroyed {
            Err(InvalidId)
        } else {
            Ok(ObjectInfo { id: id.id, interface: object.interface, version: object.version })
        }
    }

    /// Create a null object ID
    ///
    /// This object ID is always invalid, and can be used as placeholder.
    pub fn null_id(&mut self) -> ObjectId {
        ObjectId { serial: 0, id: 0, interface: &ANONYMOUS_INTERFACE }
    }

    /// Create a placeholder ID for object creation
    ///
    /// This ID needs to be created beforehand and given as argument to a request creating a
    /// new object ID. A specification must be specified if the interface and version cannot
    /// be inferred from the protocol (for example object creation from the `wl_registry`).
    ///
    /// If a specification is provided it'll be checked against what can be deduced from the
    /// protocol specification, and [`send_request`](Handle::send_request) will panic if they
    /// do not match.
    pub fn placeholder_id(&mut self, spec: Option<(&'static Interface, u32)>) -> ObjectId {
        self.pending_placeholder = spec;
        ObjectId {
            serial: 0,
            id: 0,
            interface: spec.map(|(i, _)| i).unwrap_or(&ANONYMOUS_INTERFACE),
        }
    }

    /// Sends a request to the server
    ///
    /// Returns an error if the sender ID of the provided message is no longer valid.
    ///
    /// **Panic:**
    ///
    /// Several checks against the protocol specification are done, and this method will panic if they do
    /// not pass:
    ///
    /// - the message opcode must be valid for the sender interface
    /// - the argument list must match the prototype for the message associated with this opcode
    /// - if the method creates a new object, a [`placeholder_id()`](Handle::placeholder_id) must be given
    ///   in the argument list, either without a specification, or with a specification that matches the
    ///   interface and version deduced from the protocol rules
    pub fn send_request(
        &mut self,
        Message { sender_id: id, opcode, args }: Message<ObjectId>,
        data: Option<Arc<dyn ObjectData>>,
    ) -> Result<ObjectId, InvalidId> {
        let object = self.get_object(id.clone())?;
        if object.data.client_destroyed {
            return Err(InvalidId);
        }

        let message_desc = match object.interface.requests.get(opcode as usize) {
            Some(msg) => msg,
            None => {
                panic!("Unknown opcode {} for object {}@{}.", opcode, object.interface.name, id.id);
            }
        };

        if !check_for_signature(message_desc.signature, &args) {
            panic!(
                "Unexpected signature for request {}@{}.{}: expected {:?}, got {:?}.",
                object.interface.name, id.id, message_desc.name, message_desc.signature, args
            );
        }

        // Prepare the child object
        let child_spec = if message_desc
            .signature
            .iter()
            .any(|arg| matches!(arg, ArgumentType::NewId(_)))
        {
            if let Some((iface, version)) = self.pending_placeholder.take() {
                if let Some(child_interface) = message_desc.child_interface {
                    if !same_interface(child_interface, iface) {
                        panic!(
                            "Wrong placeholder used when sending request {}@{}.{}: expected interface {} but got {}",
                            object.interface.name,
                            id.id,
                            message_desc.name,
                            child_interface.name,
                            iface.name
                        );
                    }
                    if version != object.version {
                        panic!(
                            "Wrong placeholder used when sending request {}@{}.{}: expected version {} but got {}",
                            object.interface.name,
                            id.id, message_desc.name,
                            object.version,
                            version
                        );
                    }
                }
                Some((iface, version))
            } else if let Some(child_interface) = message_desc.child_interface {
                Some((child_interface, object.version))
            } else {
                panic!(
                    "Wrong placeholder used when sending request {}@{}.{}: target interface must be specified for a generic constructor.",
                    object.interface.name,
                    id.id,
                    message_desc.name
                );
            }
        } else {
            None
        };

        let child = if let Some((child_interface, child_version)) = child_spec {
            let child_serial = self.next_serial();

            let child = Object {
                interface: child_interface,
                version: child_version,
                data: Data {
                    client_destroyed: false,
                    server_destroyed: false,
                    user_data: Arc::new(DumbObjectData),
                    serial: child_serial,
                },
            };

            let child_id = self.map.client_insert_new(child);

            self.map
                .with(child_id, |obj| {
                    obj.data.user_data = data.expect(
                        "Sending a request creating an object without providing an object data.",
                    );
                })
                .unwrap();
            Some((child_id, child_serial, child_interface))
        } else {
            None
        };

        // Prepare the message in a debug-compatible way
        let args = args.into_iter().map(|arg| {
            if let Argument::NewId(p) = arg {
                if !p.id == 0 {
                    panic!("The newid provided when sending request {}@{}.{} is not a placeholder.", object.interface.name, id.id, message_desc.name);
                }
                if let Some((child_id, child_serial, child_interface)) = child {
                    Argument::NewId(ObjectId { id: child_id, serial: child_serial, interface: child_interface})
                } else {
                    unreachable!();
                }
            } else {
                arg
            }
        }).collect::<SmallVec<[_; INLINE_ARGS]>>();

        if self.debug {
            super::debug::print_send_message(
                object.interface.name,
                id.id,
                message_desc.name,
                &args,
            );
        }
        log::debug!("Sending {}.{} ({})", id, message_desc.name, DisplaySlice(&args));

        // Send the message

        let mut msg_args = SmallVec::with_capacity(args.len());
        let mut arg_interfaces = message_desc.arg_interfaces.iter();
        for (i, arg) in args.into_iter().enumerate() {
            msg_args.push(match arg {
                Argument::Array(a) => Argument::Array(a),
                Argument::Int(i) => Argument::Int(i),
                Argument::Uint(u) => Argument::Uint(u),
                Argument::Str(s) => Argument::Str(s),
                Argument::Fixed(f) => Argument::Fixed(f),
                Argument::NewId(nid) => Argument::NewId(nid.id),
                Argument::Fd(f) => Argument::Fd(f),
                Argument::Object(o) => {
                    if o.id != 0 {
                        let object = self.get_object(o.clone())?;
                        let next_interface = arg_interfaces.next().unwrap();
                        if !same_interface_or_anonymous(next_interface, object.interface) {
                            panic!("Request {}@{}.{} expects an argument of interface {} but {} was provided instead.", object.interface.name, id.id, message_desc.name, next_interface.name, object.interface.name);
                        }
                    } else if !matches!(message_desc.signature[i], ArgumentType::Object(AllowNull::Yes)) {
                        panic!("Request {}@{}.{} expects an non-null object argument.", object.interface.name, id.id, message_desc.name);
                    }
                    Argument::Object(o.id)
                }
            });
        }

        let msg = Message { sender_id: id.id, opcode, args: msg_args };

        if let Err(err) = self.socket.write_message(&msg) {
            self.last_error = Some(WaylandError::Io(err));
        }

        // Handle destruction if relevant
        if message_desc.is_destructor {
            self.map
                .with(id.id, |obj| {
                    obj.data.client_destroyed = true;
                })
                .unwrap();
            object.data.user_data.destroyed(id);
        }
        if let Some((child_id, child_serial, child_interface)) = child {
            Ok(ObjectId { id: child_id, serial: child_serial, interface: child_interface })
        } else {
            Ok(self.null_id())
        }
    }

    /// Access the object data associated with a given object ID
    ///
    /// Returns an error if the object ID is not longer valid
    pub fn get_data(&self, id: ObjectId) -> Result<Arc<dyn ObjectData>, InvalidId> {
        let object = self.get_object(id)?;
        Ok(object.data.user_data)
    }

    /// Set the object data associated with a given object ID
    ///
    /// Returns an error if the object ID is not longer valid
    pub fn set_data(&mut self, id: ObjectId, data: Arc<dyn ObjectData>) -> Result<(), InvalidId> {
        self.map
            .with(id.id, move |objdata| {
                if objdata.data.serial != id.serial {
                    Err(InvalidId)
                } else {
                    objdata.data.user_data = data;
                    Ok(())
                }
            })
            .unwrap_or(Err(InvalidId))
    }
}

impl Handle {
    fn next_serial(&mut self) -> u32 {
        self.last_serial = self.last_serial.wrapping_add(1);
        self.last_serial
    }

    #[inline]
    fn no_last_error(&self) -> Result<(), WaylandError> {
        if let Some(ref err) = self.last_error {
            Err(err.clone())
        } else {
            Ok(())
        }
    }

    #[inline]
    fn store_and_return_error(&mut self, err: impl Into<WaylandError>) -> WaylandError {
        let err = err.into();
        log::error!("{}", err);
        self.last_error = Some(err.clone());
        err
    }

    #[inline]
    fn store_if_not_wouldblock_and_return_error(&mut self, e: std::io::Error) -> WaylandError {
        if e.kind() != std::io::ErrorKind::WouldBlock {
            self.store_and_return_error(e)
        } else {
            e.into()
        }
    }

    fn get_object(&self, id: ObjectId) -> Result<Object<Data>, InvalidId> {
        let object = self.map.find(id.id).ok_or(InvalidId)?;
        if object.data.serial != id.serial {
            return Err(InvalidId);
        }
        Ok(object)
    }

    fn handle_display_event(&mut self, message: Message<u32>) -> Result<(), WaylandError> {
        match message.opcode {
            0 => {
                // wl_display.error
                if let [Argument::Object(obj), Argument::Uint(code), Argument::Str(ref message)] =
                    message.args[..]
                {
                    let object = self.map.find(obj);
                    let err = WaylandError::Protocol(ProtocolError {
                        code,
                        object_id: obj,
                        object_interface: object
                            .map(|obj| obj.interface.name)
                            .unwrap_or("<unknown>")
                            .into(),
                        message: message.to_string_lossy().into(),
                    });
                    return Err(self.store_and_return_error(err));
                } else {
                    unreachable!()
                }
            }
            1 => {
                // wl_display.delete_id
                if let [Argument::Uint(id)] = message.args[..] {
                    let client_destroyed = self
                        .map
                        .with(id, |obj| {
                            obj.data.server_destroyed = true;
                            obj.data.client_destroyed
                        })
                        .unwrap_or(false);
                    if client_destroyed {
                        self.map.remove(id);
                    }
                } else {
                    unreachable!()
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }
}

struct DumbObjectData;

impl ObjectData for DumbObjectData {
    fn event(
        self: Arc<Self>,
        _handle: &mut Handle,
        _msg: Message<ObjectId>,
    ) -> Option<Arc<dyn ObjectData>> {
        unreachable!()
    }

    fn destroyed(&self, _object_id: ObjectId) {
        unreachable!()
    }
}

struct UninitObjectData;

impl ObjectData for UninitObjectData {
    fn event(
        self: Arc<Self>,
        _handle: &mut Handle,
        msg: Message<ObjectId>,
    ) -> Option<Arc<dyn ObjectData>> {
        panic!("Received a message on an uninitialized object: {:?}", msg);
    }

    fn destroyed(&self, _object_id: ObjectId) {}

    fn debug(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UninitObjectData").finish()
    }
}
