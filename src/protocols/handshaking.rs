use crate::{
    connections::{ConnectionReader, ConnectionSide, ConnectionWriter},
    Pea2Pea,
};

use tokio::sync::{mpsc, oneshot};

use std::io;

/// Can be used to specify and enable network handshakes. Upon establishing a connection, both sides will
/// need to adhere to the specified handshake rules in order to finalize the connection and be able to send
/// or receive any messages.
pub trait Handshaking: Pea2Pea {
    /// Prepares the node to perform specified network handshakes.
    fn enable_handshaking(&self);
}

/// A set of objects required to enable the `Handshaking` protocol.
pub type HandshakeObjects = (
    ConnectionReader,
    ConnectionWriter,
    ConnectionSide,
    oneshot::Sender<io::Result<(ConnectionReader, ConnectionWriter)>>,
);

/// An object dedicated to handling connection handshakes; used in the `Handshaking` protocol.
pub struct HandshakeHandler(mpsc::Sender<HandshakeObjects>);

impl HandshakeHandler {
    /// Sends handshake-relevant objects to the handshake handler.
    pub async fn send(&self, handshake_objects: HandshakeObjects) {
        if self.0.send(handshake_objects).await.is_err() {
            // can't recover if this happens
            panic!("HandshakeHandler's Receiver is closed")
        }
    }
}

impl From<mpsc::Sender<HandshakeObjects>> for HandshakeHandler {
    fn from(sender: mpsc::Sender<HandshakeObjects>) -> Self {
        Self(sender)
    }
}
