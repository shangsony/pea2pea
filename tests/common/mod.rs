use tokio::io::AsyncReadExt;
use tracing::*;

use pea2pea::{
    ConnectionReader, ContainsNode, MessagingProtocol, Node, NodeConfig, PacketingProtocol,
};

use std::{convert::TryInto, net::SocketAddr, sync::Arc};

#[derive(Clone)]
pub struct RandomNode(pub Arc<Node>);

impl RandomNode {
    #[allow(dead_code)]
    pub async fn new<T: AsRef<str>>(name: T) -> Self {
        let mut config = NodeConfig::default();
        config.name = Some(name.as_ref().into());
        Self(Node::new(Some(config)).await.unwrap())
    }
}

impl ContainsNode for RandomNode {
    fn node(&self) -> &Arc<Node> {
        &self.0
    }
}

#[macro_export]
macro_rules! impl_messaging_protocol {
    ($target: ty) => {
        #[async_trait::async_trait]
        impl MessagingProtocol for $target {
            type Message = ();

            async fn receive_message(
                connection_reader: &mut ConnectionReader,
            ) -> std::io::Result<&[u8]> {
                // expecting the test messages to be prefixed with their length encoded as a LE u16
                let msg_len_size: usize = 2;

                let buffer = &mut connection_reader.buffer;
                connection_reader
                    .reader
                    .read_exact(&mut buffer[..msg_len_size])
                    .await?;
                let msg_len =
                    u16::from_le_bytes(buffer[..msg_len_size].try_into().unwrap()) as usize;
                connection_reader
                    .reader
                    .read_exact(&mut buffer[..msg_len])
                    .await?;

                Ok(&buffer[..msg_len])
            }

            fn parse_message(&self, _source: SocketAddr, _message: &[u8]) -> Option<Self::Message> {
                Some(())
            }

            fn process_message(&self, source: SocketAddr, _message: &Self::Message) {
                info!(parent: self.node().span(), "received a message from {}", source);
            }
        }
    };
}

impl_messaging_protocol!(RandomNode);

// prepend the message with its length in LE u16
pub fn packeting_closure(message: &mut Vec<u8>) {
    let u16_len_bytes = (message.len() as u16).to_le_bytes();
    message.extend_from_slice(&u16_len_bytes);
    message.rotate_right(2);
}

impl PacketingProtocol for RandomNode {
    fn enable_packeting_protocol(&self) {
        self.node()
            .set_packeting_closure(Box::new(packeting_closure));
    }
}
