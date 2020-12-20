use parking_lot::RwLock;
use tokio::sync::Mutex;

use crate::connection::Connection;

use std::{collections::HashMap, io, net::SocketAddr, sync::Arc};

type ConnectionMap = HashMap<SocketAddr, Arc<Mutex<Connection>>>;

#[derive(Default)]
pub(crate) struct Connections {
    pub(crate) handshaking: RwLock<ConnectionMap>,
    pub(crate) handshaken: RwLock<ConnectionMap>,
}

impl Connections {
    pub(crate) fn is_connected(&self, addr: SocketAddr) -> bool {
        self.is_handshaking(addr) || self.is_handshaken(addr)
    }

    pub(crate) fn is_handshaking(&self, addr: SocketAddr) -> bool {
        self.handshaking.read().contains_key(&addr)
    }

    pub(crate) fn is_handshaken(&self, addr: SocketAddr) -> bool {
        self.handshaken.read().contains_key(&addr)
    }

    pub(crate) fn disconnect(&self, addr: SocketAddr) -> bool {
        if self.handshaking.write().remove(&addr).is_none() {
            self.handshaken.write().remove(&addr).is_some()
        } else {
            true
        }
    }

    pub(crate) fn num_connected(&self) -> usize {
        self.handshaking.read().len() + self.handshaken.read().len()
    }

    pub(crate) fn handshaken_connections(&self) -> Vec<(SocketAddr, Arc<Mutex<Connection>>)> {
        self.handshaken
            .read()
            .iter()
            .map(|(addr, conn)| (*addr, Arc::clone(conn)))
            .collect()
    }

    pub(crate) fn mark_as_handshaken(&self, addr: SocketAddr) -> io::Result<()> {
        if let Some(conn) = self.handshaking.write().remove(&addr) {
            self.handshaken.write().insert(addr, conn);
            Ok(())
        } else {
            Err(io::ErrorKind::NotConnected.into())
        }
    }

    pub(crate) async fn send_direct_message(
        &self,
        target: SocketAddr,
        message: Vec<u8>,
    ) -> io::Result<()> {
        let conn = self.handshaken.read().get(&target).cloned();

        let mut conn = if conn.is_some() {
            conn
        } else {
            self.handshaking.read().get(&target).cloned()
        };

        if let Some(ref mut conn) = conn {
            conn.lock().await.send_message(message).await
        } else {
            Err(io::ErrorKind::NotConnected.into())
        }
    }
}
