use crate::Node;

use once_cell::sync::OnceCell;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::tcp::{OwnedReadHalf, OwnedWriteHalf},
    sync::Mutex,
    task::JoinHandle,
};
use tracing::*;

use std::{
    io::{self, ErrorKind},
    ops::Not,
    sync::Arc,
};

#[derive(Clone, Copy)]
pub(crate) enum ConnectionSide {
    Initiator,
    Responder,
}

impl Not for ConnectionSide {
    type Output = Self;

    fn not(self) -> Self::Output {
        match self {
            Self::Initiator => Self::Responder,
            Self::Responder => Self::Initiator,
        }
    }
}

// FIXME: these pubs are not ideal
pub struct ConnectionReader {
    pub node: Arc<Node>,
    pub buffer: Vec<u8>,
    pub reader: OwnedReadHalf,
}

impl ConnectionReader {
    pub(crate) fn new(reader: OwnedReadHalf, node: Arc<Node>) -> Self {
        Self {
            buffer: vec![0; node.config.conn_read_buffer_size],
            node,
            reader,
        }
    }

    // FIXME: this pub is not ideal
    pub async fn read_bytes(&mut self, num: usize) -> io::Result<usize> {
        let buffer = &mut self.buffer;

        if num > buffer.len() {
            error!(parent: self.node.span(), "can' read {}B from the stream; the buffer is too small ({}B)", num, buffer.len());
            return Err(ErrorKind::Other.into());
        }

        self.reader.read_exact(&mut buffer[..num]).await
    }
}

pub struct Connection {
    node: Arc<Node>,
    pub(crate) reader_task: OnceCell<Option<JoinHandle<()>>>,
    writer: Mutex<OwnedWriteHalf>,
    side: ConnectionSide,
}

impl Connection {
    pub(crate) fn new(writer: OwnedWriteHalf, node: Arc<Node>, side: ConnectionSide) -> Self {
        Self {
            node,
            reader_task: Default::default(),
            writer: Mutex::new(writer),
            side,
        }
    }

    pub(crate) async fn send_message(&self, mut message: Vec<u8>) -> io::Result<()> {
        if let Some(packeting_closure) = self.node.packeting_closure() {
            packeting_closure(&mut message);
            self.write_bytes(&message).await
        } else {
            self.write_bytes(&message).await
        }
    }

    // FIXME: this pub is not ideal
    pub async fn write_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        let mut writer = self.writer.lock().await;
        writer.write(bytes).await?;
        writer.flush().await
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Ok(peer_addr) = self.writer.get_mut().as_ref().peer_addr() {
            debug!(parent: self.node.span(), "disconnecting from {}", peer_addr);
            // if the main node was not the initiator of the connection, it doesn't know the listening address
            // of the associated peer, so the related stats are unreliable; the next connection initiated by
            // the peer could be bound to an entirely different port number
            if matches!(self.side, ConnectionSide::Initiator) {
                self.node
                    .known_peers
                    .peer_stats()
                    .write()
                    .remove(&peer_addr);
            }
        } else {
            warn!(parent: self.node.span(), "couldn't remove the stats of an obsolete peer");
        }
    }
}
