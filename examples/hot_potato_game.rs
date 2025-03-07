mod common;

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rand::{rngs::SmallRng, seq::IteratorRandom, Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::sleep,
};
use tracing::*;
use tracing_subscriber::filter::LevelFilter;

use pea2pea::{
    connect_nodes,
    protocols::{Handshake, Reading, Writing},
    Connection, ConnectionSide, Node, Pea2Pea, Topology,
};

use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering::Relaxed},
        Arc,
    },
    time::Duration,
};

static RNG: Lazy<Mutex<SmallRng>> = Lazy::new(|| Mutex::new(SmallRng::from_entropy()));

type PlayerName = String;

#[derive(Debug)]
struct PlayerInfo {
    addr: SocketAddr,
    is_carrier: bool,
}

#[derive(Clone)]
struct Player {
    node: Node,
    other_players: Arc<Mutex<HashMap<PlayerName, PlayerInfo>>>,
    potato_count: Arc<AtomicUsize>,
}

impl Player {
    async fn new() -> Self {
        Self {
            node: Node::new(None).await.unwrap(),
            other_players: Default::default(),
            potato_count: Default::default(),
        }
    }

    async fn throw_potato(&self) {
        let message = Message::IHaveThePotato(self.node().name().into());
        self.send_broadcast(message).unwrap();

        let (new_carrier_name, new_carrier_addr) = self
            .other_players
            .lock()
            .iter()
            .map(|(name, player)| (name.clone(), player.addr))
            .choose(&mut *RNG.lock())
            .unwrap();

        info!(parent: self.node().span(), "throwing the potato to player {}!", new_carrier_name);

        self.send_direct_message(new_carrier_addr, Message::HotPotato)
            .unwrap()
            .await
            .unwrap();
    }
}

impl Pea2Pea for Player {
    fn node(&self) -> &Node {
        &self.node
    }
}

#[async_trait::async_trait]
impl Handshake for Player {
    async fn perform_handshake(&self, mut conn: Connection) -> io::Result<Connection> {
        let mut buffer = [0u8; 16];

        let peer_name = match !conn.side {
            ConnectionSide::Initiator => {
                // send own PlayerName
                let own_name = self.node().name().as_bytes().to_vec();
                conn.writer().write_all(&own_name).await?;

                // receive the peer's PlayerName
                let len = conn.reader().read(&mut buffer).await?;

                String::from_utf8_lossy(&buffer[..len]).into_owned()
            }
            ConnectionSide::Responder => {
                // receive the peer's PlayerName
                let len = conn.reader().read(&mut buffer).await?;
                let peer_name = String::from_utf8_lossy(&buffer[..len]).into_owned();

                // send own PlayerName
                let own_name = self.node().name().as_bytes().to_vec();
                conn.writer().write_all(&own_name).await?;

                peer_name
            }
        };

        let player = PlayerInfo {
            addr: conn.addr,
            is_carrier: false,
        };
        self.other_players.lock().insert(peer_name, player);

        Ok(conn)
    }
}

#[derive(Serialize, Deserialize, Clone)]
enum Message {
    HotPotato,
    IHaveThePotato(PlayerName),
}

#[async_trait::async_trait]
impl Reading for Player {
    type Message = Message;

    fn read_message<R: io::Read>(
        &self,
        _source: SocketAddr,
        reader: &mut R,
    ) -> io::Result<Option<Self::Message>> {
        // expecting inbound messages to be prefixed with their length encoded as a LE u16
        let vec = common::read_len_prefixed_message::<R, 2>(reader)?;

        vec.map(|v| bincode::deserialize(&v).map_err(|_| io::ErrorKind::InvalidData.into()))
            .transpose()
    }

    async fn process_message(&self, _source: SocketAddr, message: Self::Message) -> io::Result<()> {
        match message {
            Message::HotPotato => {
                info!(parent: self.node().span(), "I have the potato!");
                {
                    let mut other_players = self.other_players.lock();
                    if let Some(old_carrier) = other_players.values_mut().find(|p| p.is_carrier) {
                        old_carrier.is_carrier = false;
                    }
                    assert!(other_players.values().all(|p| !p.is_carrier));
                }

                self.potato_count.fetch_add(1, Relaxed);
                self.throw_potato().await;
            }
            Message::IHaveThePotato(carrier) => {
                let mut other_players = self.other_players.lock();

                if let Some(old_carrier) = other_players.values_mut().find(|p| p.is_carrier) {
                    old_carrier.is_carrier = false;
                }
                assert!(other_players.values().all(|p| !p.is_carrier));
                if let Some(new_carrier) = other_players.get_mut(&carrier) {
                    new_carrier.is_carrier = true;
                }
            }
        }

        Ok(())
    }
}

impl Writing for Player {
    type Message = Message;

    fn write_message<W: io::Write>(
        &self,
        _: SocketAddr,
        payload: &Self::Message,
        writer: &mut W,
    ) -> io::Result<()> {
        let payload_len = bincode::serialized_size(payload).unwrap() as u16;
        writer.write_all(&payload_len.to_le_bytes())?;
        bincode::serialize_into(writer, payload)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }
}

#[tokio::main]
async fn main() {
    common::start_logger(LevelFilter::OFF);

    const GAME_TIME_SECS: u64 = 5;
    const NUM_PLAYERS: usize = 10;

    println!(
        "hot potato! players: {}, play time: {}s",
        NUM_PLAYERS, GAME_TIME_SECS
    );

    let mut players = Vec::with_capacity(NUM_PLAYERS);
    for _ in 0..NUM_PLAYERS {
        players.push(Player::new().await);
    }

    for player in &players {
        player.enable_handshake().await;
        player.enable_reading().await;
        player.enable_writing().await;
    }
    connect_nodes(&players, Topology::Mesh).await.unwrap();

    let first_carrier = RNG.lock().gen_range(0..NUM_PLAYERS);
    players[first_carrier].potato_count.fetch_add(1, Relaxed);
    players[first_carrier].throw_potato().await;

    sleep(Duration::from_secs(GAME_TIME_SECS)).await;

    println!("\n---------- scoreboard ----------");
    for player in &players {
        println!(
            "player {} got the potato {} times",
            player.node().name(),
            player.potato_count.load(Relaxed)
        );
    }
}
