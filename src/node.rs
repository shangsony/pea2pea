use crate::config::NodeConfig;
use crate::connection::{Connection, ConnectionReader, ConnectionSide};
use crate::connections::Connections;
use crate::known_peers::KnownPeers;
use crate::protocols::{
    HandshakeSetup, InboundMessages, PacketingClosure, Protocols, ReadingClosure,
};

use tokio::net::{TcpListener, TcpStream};
use tracing::*;

use std::{
    io::{self, ErrorKind},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

static SEQUENTIAL_NODE_ID: AtomicUsize = AtomicUsize::new(0);

pub trait ContainsNode {
    fn node(&self) -> &Arc<Node>;
}

pub struct Node {
    span: Span,
    pub config: NodeConfig,
    pub listening_addr: SocketAddr,
    protocols: Protocols,
    connections: Connections,
    pub known_peers: KnownPeers,
}

impl Node {
    pub async fn new(config: Option<NodeConfig>) -> io::Result<Arc<Self>> {
        let local_ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let mut config = config.unwrap_or_default();

        if config.name.is_none() {
            config.name = Some(
                SEQUENTIAL_NODE_ID
                    .fetch_add(1, Ordering::SeqCst)
                    .to_string(),
            );
        }

        let span = create_span(config.name.as_deref().unwrap());

        let desired_listener = if let Some(port) = config.desired_listening_port {
            let desired_listening_addr = SocketAddr::new(local_ip, port);
            TcpListener::bind(desired_listening_addr).await
        } else if config.allow_random_port {
            let random_available_addr = SocketAddr::new(local_ip, 0);
            TcpListener::bind(random_available_addr).await
        } else {
            panic!("you must either provide a desired port or allow a random port to be chosen");
        };

        let listener = match desired_listener {
            Ok(listener) => listener,
            Err(e) => {
                if config.allow_random_port {
                    warn!(parent: span.clone(), "trying any port, the desired one is unavailable: {}", e);
                    let random_available_addr = SocketAddr::new(local_ip, 0);
                    TcpListener::bind(random_available_addr).await?
                } else {
                    error!(parent: span.clone(), "the desired port is unavailable: {}", e);
                    return Err(e);
                }
            }
        };
        let listening_addr = listener.local_addr()?;

        let node = Arc::new(Self {
            span,
            config,
            listening_addr,
            protocols: Default::default(),
            connections: Default::default(),
            known_peers: Default::default(),
        });

        let node_clone = Arc::clone(&node);
        tokio::spawn(async move {
            debug!(parent: node_clone.span(), "spawned a listening task");
            loop {
                match listener.accept().await {
                    Ok((stream, addr)) => {
                        if let Err(e) = Arc::clone(&node_clone)
                            .accept_connection(stream, addr)
                            .await
                        {
                            error!(parent: node_clone.span(), "couldn't accept a connection: {}", e);
                        }
                    }
                    Err(e) => {
                        error!(parent: node_clone.span(), "couldn't accept a connection: {}", e);
                    }
                }
            }
        });

        info!(
            parent: node.span(),
            "the node is ready; listening on {}",
            listening_addr
        );

        Ok(node)
    }

    pub async fn spawn_multiple(
        count: usize,
        config: Option<NodeConfig>,
    ) -> io::Result<Vec<Arc<Self>>> {
        let mut nodes = Vec::with_capacity(count);

        for _ in 0..count {
            let node = Node::new(config.clone()).await?;
            nodes.push(node);
        }

        Ok(nodes)
    }

    pub fn name(&self) -> &str {
        // safe; can be set as None in NodeConfig, but receives a default value on Node creation
        self.config.name.as_deref().unwrap()
    }

    pub fn span(&self) -> Span {
        self.span.clone()
    }

    async fn adapt_stream(
        self: &Arc<Self>,
        stream: TcpStream,
        peer_addr: SocketAddr,
        own_side: ConnectionSide,
    ) -> io::Result<()> {
        debug!(parent: self.span(), "establishing connection with {}", peer_addr);

        // check the local address of the connection; it will differ from peer_addr
        // if the Node accepted the connection, as opposed to having initiated it
        let peer_local_addr = if let ConnectionSide::Responder = own_side {
            peer_addr
        } else if let Ok(addr) = stream.local_addr() {
            addr
        } else {
            error!(parent: self.span(), "couldn't determine the local address of the peer");
            return Err(ErrorKind::Other.into());
        };

        let (reader, writer) = stream.into_split();

        let connection_reader = ConnectionReader::new(reader, Arc::clone(&self));
        let connection = Arc::new(Connection::new(writer, Arc::clone(&self), !own_side));

        self.connections
            .handshaking
            .write()
            .insert(peer_addr, Arc::clone(&connection));

        let connection_reader = if let Some(ref handshake_setup) = self.handshake_setup() {
            let handshake_task = match own_side {
                ConnectionSide::Initiator => {
                    (handshake_setup.initiator_closure)(peer_addr, connection_reader, connection)
                }
                ConnectionSide::Responder => {
                    (handshake_setup.responder_closure)(peer_addr, connection_reader, connection)
                }
            };

            match handshake_task.await {
                Ok(Ok((conn_reader, handshake_state))) => {
                    if let Some(ref sender) = handshake_setup.state_sender {
                        if let Err(e) = sender.send((peer_addr, handshake_state)).await {
                            error!(parent: self.span(), "couldn't registed handshake state: {}", e);
                            // TODO: what to do?
                        }
                    }

                    conn_reader
                }
                _ => {
                    error!(parent: self.span(), "handshake with {} failed; dropping the connection", peer_addr);
                    self.register_failure(peer_addr);
                    return Err(ErrorKind::Other.into());
                }
            }
        } else {
            connection_reader
        };

        let reader_task = if let Some(ref messaging_closure) = self.reading_closure() {
            Some(messaging_closure(connection_reader, peer_addr))
        } else {
            None
        };

        if let Err(e) = self
            .connections
            .mark_as_handshaken(peer_addr, reader_task)
            .await
        {
            error!(parent: self.span(), "can't mark {} as handshaken: {}", peer_addr, e);
            Err(ErrorKind::Other.into())
        } else {
            debug!(parent: self.span(), "marked {}{} as handshaken", peer_addr, if peer_addr != peer_local_addr { format!(" (aka {})", peer_local_addr) } else { "".into() });
            Ok(())
        }
    }

    async fn accept_connection(
        self: Arc<Self>,
        stream: TcpStream,
        addr: SocketAddr,
    ) -> io::Result<()> {
        self.known_peers.add(addr);
        self.adapt_stream(stream, addr, ConnectionSide::Responder)
            .await
    }

    pub async fn initiate_connection(self: &Arc<Self>, addr: SocketAddr) -> io::Result<()> {
        if self.connections.is_connected(addr) {
            warn!(parent: self.span(), "already connecting/connected to {}", addr);
            return Err(ErrorKind::Other.into());
        }

        let stream = TcpStream::connect(addr).await?;
        self.known_peers.add(addr);
        self.adapt_stream(stream, addr, ConnectionSide::Initiator)
            .await
    }

    pub fn disconnect(&self, addr: SocketAddr) -> bool {
        let disconnected = self.connections.disconnect(addr);

        if disconnected {
            info!(parent: self.span(), "disconnected from {}", addr);
        } else {
            warn!(parent: self.span(), "wasn't connected to {}", addr);
        }

        disconnected
    }

    pub async fn send_direct_message(&self, addr: SocketAddr, message: Vec<u8>) -> io::Result<()> {
        let ret = self.connections.send_direct_message(addr, message).await;

        if let Err(ref e) = ret {
            error!(parent: self.span(), "couldn't send a direct message to {}: {}", addr, e);
        }

        ret
    }

    pub async fn send_broadcast(&self, message: Vec<u8>) {
        for (addr, conn) in self.connections.handshaken_connections().iter() {
            // FIXME: it would be nice not to clone the message
            if let Err(e) = conn.send_message(message.clone()).await {
                error!(parent: self.span(), "couldn't send a broadcast to {}: {}", addr, e);
            }
        }
    }

    pub fn handshaken_addrs(&self) -> Vec<SocketAddr> {
        self.connections.handshaken.read().keys().copied().collect()
    }

    pub fn register_received_message(&self, from: SocketAddr, len: usize) {
        self.known_peers.register_received_message(from, len)
    }

    pub fn register_failure(&self, from: SocketAddr) {
        self.known_peers.register_failure(from)
    }

    pub fn is_connected(&self, addr: SocketAddr) -> bool {
        self.connections.is_connected(addr)
    }

    pub fn num_connected(&self) -> usize {
        self.connections.num_connected()
    }

    pub fn is_handshaking(&self, addr: SocketAddr) -> bool {
        self.connections.is_handshaking(addr)
    }

    pub fn is_handshaken(&self, addr: SocketAddr) -> bool {
        self.connections.is_handshaken(addr)
    }

    pub fn num_messages_received(&self) -> usize {
        self.known_peers.num_messages_received()
    }

    pub async fn mark_as_handshaken(&self, addr: SocketAddr) -> io::Result<()> {
        self.connections.mark_as_handshaken(addr, None).await
    }

    pub fn inbound_messages(&self) -> Option<&InboundMessages> {
        self.protocols.inbound_messages.get()
    }

    pub fn reading_closure(&self) -> Option<&ReadingClosure> {
        self.protocols.reading_closure.get()
    }

    pub fn packeting_closure(&self) -> Option<&PacketingClosure> {
        self.protocols.packeting_closure.get()
    }

    pub fn handshake_setup(&self) -> Option<&HandshakeSetup> {
        self.protocols.handshake_setup.get()
    }

    pub fn set_inbound_messages(&self, sender: InboundMessages) {
        self.protocols
            .inbound_messages
            .set(sender)
            .expect("the inbound_messages field was set more than once!");
    }

    pub fn set_reading_closure(&self, closure: ReadingClosure) {
        if self.protocols.reading_closure.set(closure).is_err() {
            panic!("the reading_closure field was set more than once!");
        }
    }

    pub fn set_packeting_closure(&self, closure: PacketingClosure) {
        if self.protocols.packeting_closure.set(closure).is_err() {
            panic!("the packeting_closure field was set more than once!");
        }
    }

    pub fn set_handshake_setup(&self, closures: HandshakeSetup) {
        if self.protocols.handshake_setup.set(closures).is_err() {
            panic!("the handshake_setup field was set more than once!");
        }
    }
}

impl ContainsNode for Arc<Node> {
    fn node(&self) -> &Arc<Node> {
        &self
    }
}

// FIXME: this can probably be done more elegantly
fn create_span(node_name: &str) -> Span {
    let span = trace_span!("node", name = node_name);
    let span = if span.is_disabled() {
        debug_span!("node", name = node_name)
    } else {
        span
    };
    let span = if span.is_disabled() {
        info_span!("node", name = node_name)
    } else {
        span
    };
    let span = if span.is_disabled() {
        warn_span!("node", name = node_name)
    } else {
        span
    };
    let span = if span.is_disabled() {
        error_span!("node", name = node_name)
    } else {
        span
    };
    span
}
