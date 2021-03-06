use std::net::{SocketAddr};
use std::collections::HashMap;
use std::sync::{Arc,RwLock,RwLockReadGuard,RwLockWriteGuard};
use interfaces::{Interface,Kind};
use futures::{Future,Poll,Async,future,Stream,Sink};
use futures::sync::mpsc::{channel,Sender};
use tokio_core::net::TcpStream;
use rustls::{ClientConfig,Certificate,ProtocolVersion};
use tokio_rustls::{ClientConfigExt};
use tokio_core::reactor::{Handle};
use tokio_uds::{UnixDatagram};
use bytes::BytesMut;
use std::io;

use transport::{Transport};
use peer_information_base::{PeerInformationBase};
use configuration::{Configuration};
use unix_socket::{ControlProtocolCodec,Raw};

#[allow(dead_code)]
struct LocalAddress {
    interface: String,
    internal_address: SocketAddr,
    external_address: Option<SocketAddr>,
}

impl LocalAddress {
    fn new<S: Into<String>>(interface: S, internal_address: SocketAddr, external_address: Option<SocketAddr>) -> LocalAddress {
        LocalAddress {
            interface: interface.into(),
            internal_address: internal_address,
            external_address: external_address,
        }
    }
}

pub struct InnerState {
    pub id: String,
    pub pib: PeerInformationBase,
    connections: HashMap<String, Vec<Transport>>,
    pub relays: Vec<String>,
    addresses: Vec<LocalAddress>,
    sockets: HashMap<(String, u16), Sender<BytesMut>>,
    handle: Handle,
}



#[derive(Clone)]
pub struct State(pub Arc<RwLock<InnerState>>);

impl State {
    pub fn from_configuration(config: Configuration, handle: Handle) -> State {
        State(Arc::new(RwLock::new(InnerState {
            id: config.id,
            pib: config.pib,
            connections: HashMap::new(),
            relays: config.relays,
            addresses: Vec::new(),
            sockets: HashMap::new(),
            handle: handle,
        })))
    }

    fn read(&self) -> RwLockReadGuard<InnerState> {
        self.0.read().expect("Unable to acquire read lock on state")
    }

    pub fn handle(&self) -> Handle {
        self.read().handle.clone()
    }

    fn write(&self) -> RwLockWriteGuard<InnerState> {
        self.0.write().expect("Unable to acquire write lock on state")
    }

    fn discover_addresses(&self) -> () {
        let mut state = self.write();
        let interfaces = match Interface::get_all()  {
            Ok(i) => i,
            Err(_) => return,
        };
        state.addresses.clear();
        for interface in interfaces {
            if interface.is_loopback() || !interface.is_up() {
                continue
            }
            for address in &interface.addresses {
                let addr = match address.addr {
                    Some(addr) => addr,
                    None => continue,
                };
                if address.kind == Kind::Ipv4 || address.kind == Kind::Ipv6 {
                    state.addresses
                        .push(LocalAddress::new(interface.name.clone(), addr, None))
                }
            }
        };
    }

    fn lookup_peer(&self, id: &str) -> Option<(SocketAddr, Certificate)> {
        self.read().pib
            .get_peer(id)
            .and_then(|peer| {
                if !peer.addresses.is_empty() {
                    Some( (peer.addresses[0], peer.user_certificate.clone()) )
                } else {
                    None
                }
            })
    }

    fn connect_to_relays(&self) {
        for relay in &self.read().relays {
            let (addr, cert) = match self.lookup_peer(relay) {
                Some(info) => info,
                None => continue
            };
            let relay = relay.clone();
            println!("Connecting to relay {}", relay);
            let future = self.connect(relay.clone(), addr, cert)
                .and_then(|_| future::ok(()) )
                .map_err(move |err| println!("Unable to connect to peer {}: {}", relay, err) );
            self.read().handle.spawn(future);
        }
    }

    fn open_ctl_socket(&self) {
        let state = self.clone();
        let done = UnixDatagram::bind("/run/user/1000/uip/ctl.sock", &self.read().handle)
            .expect("Unable to open unix control socket")
            .framed(ControlProtocolCodec)
            .for_each(move |(path, host_id, channel_id)| {
                let socket = UnixDatagram::unbound(&state.read().handle)?;
                socket.connect(path)?;
                let (sink, stream) = socket.framed(Raw).split();
                let (sender, receiver) = channel::<BytesMut>(10);
                state.read().handle.spawn(receiver.forward(sink.sink_map_err(|_|())).map(|_| ()).map_err(|_| ()));
                state.write().sockets.insert( (host_id.clone(), channel_id), sender);
                let state2 = state.clone();
                let done = stream.for_each(move |buf| {
                    state2.send_frame(host_id.clone(), channel_id, buf);
                    future::ok(())
                }).map_err(|_| ());
                state.read().handle.spawn(done);
                Ok(())
            }).map_err(|e| println!("Control socket was closed: {}", e) );
        self.read().handle.spawn(done);
    }

    fn add_connection(&self, id: String, conn: Transport) {
        self.write()
            .connections.entry(id).or_insert_with(Vec::new)
            .push(conn);
    }

    fn connect(&self, id: String, addr: SocketAddr, cert: Certificate) -> impl Future<Item=Transport, Error=io::Error> {
        let handle = self.read().handle.clone();
        let config = {
            let mut config = ClientConfig::new();
            config.versions = vec![ProtocolVersion::TLSv1_2];
            let _ = config.root_store.add(&cert);
            Arc::new(config)
        };
        let state = self.clone();
        let id2 = id.clone();
        TcpStream::connect(&addr, &handle)
            .and_then(move |stream| config.connect_async(id.as_ref(), stream) )
            .and_then(move |stream| {
                let conn = Transport::from_tls_stream(state.clone(), stream, id2.clone());
                state.add_connection(id2, conn.clone());
                Ok(conn)
            })
    }

    pub fn send_frame(&self, host_id: String, channel_id: u16, data: BytesMut) {
        if let Some(connections) = self.read().connections.get(&host_id) {
            if let Some(connection) = connections.first() {
                connection.send_frame(channel_id, data);
            }
        }
    }

    pub fn deliver_frame(&self, host_id: String, channel_id: u16, data: BytesMut) {
        if let Some(socket) = self.read().sockets.get( &(host_id, channel_id) ) {
            self.read().handle.spawn(socket.clone().send(data).map(|_| ()).map_err(|_| ()));
        }
    }
}

impl Future for State {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        self.discover_addresses();
        self.connect_to_relays();
        self.open_ctl_socket();
        Ok(Async::NotReady)
    }
}
