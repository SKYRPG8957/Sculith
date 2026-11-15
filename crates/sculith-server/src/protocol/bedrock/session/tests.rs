use std::{collections::VecDeque, net::SocketAddr, time::Duration};

use anyhow::{Context, Result, ensure};
use bedrock::{
    auth::ClientData,
    network::{
        login::{ClientIdentity, LoginOptions, NetworkSettings},
        tokio::{
            Client, Connection,
            error::TransportLayerError,
            transport::{Transport, TransportLayerConnection},
        },
    },
    protocol::{
        ProtoVersion, V2225,
        v662::{
            enums::PlayStatus,
            packets::{
                ClientCacheStatusPacket, ClientToServerHandshakePacket, LoginPacket,
                RequestChunkRadiusPacket, RequestNetworkSettingsPacket,
                SetLocalPlayerAsInitializedPacket,
            },
            types::ActorRuntimeID,
        },
        v2168::packets::ResourcePackClientResponsePacket,
    },
};
use sculith_core::ServerStatus;
use tokio::{net::UdpSocket, sync::watch, task::JoinHandle};

use super::Authentication;

struct Harness {
    address: SocketAddr,
    shutdown: watch::Sender<bool>,
    _status: watch::Sender<ServerStatus>,
    task: JoinHandle<Result<()>>,
    world: crate::world::WorldHandle,
}

impl Harness {
    async fn start(authentication: Authentication, timeout: Duration) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let (status, receiver) =
            watch::channel(ServerStatus::new("session tests".into(), 20).unwrap());
        let (shutdown, stop) = watch::channel(false);
        let (world, owner) = crate::world::create(stop.clone());
        let connection_world = world.clone();
        let task = tokio::spawn(async move {
            let (driver, owner) = tokio::join!(
                super::super::transport::serve(
                    socket,
                    receiver,
                    9876,
                    authentication,
                    timeout,
                    stop,
                    connection_world,
                ),
                owner
            );
            driver?;
            owner
        });
        Self {
            address,
            shutdown,
            _status: status,
            task,
            world,
        }
    }

    async fn stop(self) {
        self.shutdown.send_replace(true);
        tokio::time::timeout(Duration::from_secs(2), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        // The driver and all accepted sessions must release the original port.
        let _rebound = UdpSocket::bind(self.address).await.unwrap();
    }
}

struct TestClient<T: Transport = TransportLayerConnection> {
    conn: Connection<V2225, T>,
    pending: VecDeque<V2225>,
}

impl<T: Transport> TestClient<T> {
    async fn enter(&mut self) -> Result<u64> {
        self.encrypted_login().await?;
        self.finish_packs().await?;
        let id = self.receive_spawn().await?;
        self.conn
            .send(&[V2225::SetLocalPlayerAsInitializedPacket(Box::new(
                SetLocalPlayerAsInitializedPacket {
                    player_id: ActorRuntimeID(id),
                },
            ))])
            .await?;
        Ok(id)
    }

    async fn request_break(&mut self, id: u64, x: i32, y: i32, z: i32) -> Result<()> {
        use bedrock::protocol::{
            v662::{enums::PlayerActionType, packets::PlayerActionPacket},
            v944::types::NetworkBlockPosition,
        };
        self.conn
            .send(&[V2225::PlayerActionPacket(Box::new(PlayerActionPacket {
                player_runtime_id: ActorRuntimeID(id),
                action: PlayerActionType::PredictDestroyBlock,
                block_position: NetworkBlockPosition { x, y, z },
                result_pos: NetworkBlockPosition { x, y, z },
                face: 1,
            }))])
            .await?;
        Ok(())
    }
    async fn next(&mut self) -> Result<V2225> {
        if let Some(packet) = self.pending.pop_front() {
            return Ok(packet);
        }
        let batch = tokio::time::timeout(Duration::from_secs(3), self.conn.recv())
            .await
            .context("test receive timed out")??;
        self.pending.extend(batch);
        self.pending.pop_front().context("empty test client batch")
    }

    async fn settings(&mut self) -> Result<()> {
        self.conn
            .send(&[V2225::RequestNetworkSettingsPacket(Box::new(
                RequestNetworkSettingsPacket {
                    client_network_version: V2225::PROTOCOL_VERSION as i32,
                },
            ))])
            .await?;
        let V2225::NetworkSettingsPacket(settings) = self.next().await? else {
            anyhow::bail!("expected network settings");
        };
        let settings = NetworkSettings::from_wire(
            settings.compression_algorithm as u16,
            settings.compression_threshold,
        )?;
        self.conn.enable_compression(settings.compression);
        Ok(())
    }

    fn identity(&self, name: &str) -> Result<ClientIdentity> {
        let data = ClientData::offline(
            V2225::GAME_VERSION,
            &self.conn.get_socket_addr().to_string(),
            name,
        );
        Ok(ClientIdentity::offline(
            ClientIdentity::generate_key(),
            name,
            &data,
        )?)
    }

    async fn request_login(&mut self, identity: &ClientIdentity) -> Result<()> {
        self.conn
            .send(&[V2225::LoginPacket(Box::new(LoginPacket {
                client_network_version: V2225::PROTOCOL_VERSION as i32,
                connection_request: identity.request().to_vec(),
            }))])
            .await?;
        Ok(())
    }

    async fn encrypted_login(&mut self) -> Result<()> {
        self.settings().await?;
        let identity = self.identity("SculithTests")?;
        self.request_login(&identity).await?;
        let V2225::ServerToClientHandshakePacket(handshake) = self.next().await? else {
            anyhow::bail!("expected encryption handshake");
        };
        self.conn
            .enable_encryption(identity.finish_encryption(&handshake.handshake_web_token)?);
        // Cache status shares the encrypted batch: the server must not drop it.
        self.conn
            .send(&[
                V2225::ClientToServerHandshakePacket(Box::new(ClientToServerHandshakePacket {})),
                V2225::ClientCacheStatusPacket(Box::new(ClientCacheStatusPacket {
                    is_cache_supported: false,
                })),
            ])
            .await?;
        let V2225::PlayStatusPacket(status) = self.next().await? else {
            anyhow::bail!("expected login success");
        };
        ensure!(
            matches!(status.status, PlayStatus::LoginSuccess),
            "login failed"
        );
        ensure!(self.conn.compression.is_some() && self.conn.encryption.is_some());
        Ok(())
    }

    async fn finish_packs(&mut self) -> Result<()> {
        let V2225::ResourcePacksInfoPacket(info) = self.next().await? else {
            anyhow::bail!("expected resource pack info");
        };
        ensure!(info.resource_packs.is_empty() && !info.resource_pack_required);
        self.conn
            .send(&[V2225::ResourcePackClientResponsePacket(Box::new(
                ResourcePackClientResponsePacket::DownloadingFinished,
            ))])
            .await?;
        let V2225::ResourcePackStackPacket(stack) = self.next().await? else {
            anyhow::bail!("expected resource pack stack");
        };
        ensure!(stack.addon_list.is_empty() && stack.base_game_version.0 == V2225::GAME_VERSION);
        self.conn
            .send(&[V2225::ResourcePackClientResponsePacket(Box::new(
                ResourcePackClientResponsePacket::ResourcePackStackFinished,
            ))])
            .await?;
        Ok(())
    }

    async fn receive_spawn(&mut self) -> Result<u64> {
        self.receive_spawn_with_radius(8).await
    }

    async fn receive_spawn_with_radius(&mut self, requested_radius: i32) -> Result<u64> {
        ensure!(matches!(self.next().await?, V2225::VoxelShapesPacket(_)));
        let V2225::StartGamePacket(game) = self.next().await? else {
            anyhow::bail!("expected StartGame");
        };
        ensure!(game.block_network_ids_are_hashes && game.position == (0.5, 5.62, 0.5));
        let id = game.target_runtime_id.0;
        ensure!(matches!(
            self.next().await?,
            V2225::UpdateAdventureSettingsPacket(_)
        ));
        ensure!(matches!(self.next().await?, V2225::ItemComponentPacket(_)));
        let V2225::UpdateAbilitiesPacket(abilities) = self.next().await? else {
            anyhow::bail!("expected abilities")
        };
        ensure!(abilities.data.layers[0].ability_values & ((1 << 6) | (1 << 7) | (1 << 17)) == 0);
        let V2225::InventoryContentPacket(inventory) = self.next().await? else {
            anyhow::bail!("expected inventory")
        };
        ensure!(
            inventory.slots.len() == 36 && inventory.slots[0].id == 1 && inventory.slots[1].id == 3
        );
        ensure!(matches!(self.next().await?, V2225::MobEquipmentPacket(_)));
        self.conn
            .send(&[V2225::RequestChunkRadiusPacket(Box::new(
                RequestChunkRadiusPacket {
                    chunk_radius: requested_radius,
                    max_chunk_radius: 32,
                },
            ))])
            .await?;
        let V2225::ChunkRadiusUpdatedPacket(radius) = self.next().await? else {
            anyhow::bail!("expected radius acknowledgement");
        };
        let expected_radius = requested_radius.min(super::spawn::RADIUS);
        ensure!(radius.chunk_radius == expected_radius);
        ensure!(matches!(
            self.next().await?,
            V2225::NetworkChunkPublisherUpdatePacket(_)
        ));
        let mut chunks = std::collections::BTreeSet::new();
        for _ in 0..(expected_radius * 2 + 1).pow(2) {
            let V2225::LevelChunkPacket(chunk) = self.next().await? else {
                anyhow::bail!("expected LevelChunk");
            };
            ensure!(chunk.dimension_id == 0 && chunk.sub_chunk_count == 24 && !chunk.cache_enabled);
            ensure!(chunk.serialized_chunk_data.len() == 1282);
            ensure!(
                (-expected_radius..=expected_radius).contains(&chunk.chunk_position.x)
                    && (-expected_radius..=expected_radius).contains(&chunk.chunk_position.z)
            );
            ensure!(
                chunks.insert((chunk.chunk_position.x, chunk.chunk_position.z)),
                "duplicate chunk"
            );
        }
        let V2225::PlayStatusPacket(status) = self.next().await? else {
            anyhow::bail!("expected PlayerSpawn");
        };
        ensure!(matches!(status.status, PlayStatus::PlayerSpawn));
        Ok(id)
    }
}

impl TestClient {
    async fn connect(address: SocketAddr) -> Result<Self> {
        let conn = Client::new()
            .timeout(Duration::from_secs(2))
            .connect::<V2225>(address)
            .await?;
        Ok(Self {
            conn,
            pending: VecDeque::new(),
        })
    }
}

struct TestNetherTransport {
    client: nethernet_tokio::NetherClient,
    address: SocketAddr,
    closed: std::sync::atomic::AtomicBool,
}

impl Transport for TestNetherTransport {
    async fn send(&mut self, batch: &[u8]) -> Result<(), TransportLayerError> {
        self.client
            .send(bytes::Bytes::copy_from_slice(batch))
            .await
            .map_err(|e| std::io::Error::other(e.to_string()).into())
    }
    async fn recv(&mut self) -> Result<Vec<u8>, TransportLayerError> {
        self.client
            .recv()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .map(|b| b.to_vec())
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into())
    }
    fn remote_addr(&self) -> SocketAddr {
        self.address
    }
    fn max_message_len(&self) -> usize {
        255 * 10_000
    }
    async fn close(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = self.client.close().await;
    }
    async fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Acquire)
    }
}

async fn nether_client(address: SocketAddr) -> Result<TestClient<TestNetherTransport>> {
    use nethernet_tokio::{
        ConnectionConfig, HttpSignaling, NetherClient, ServerIdentity, TokenTrust,
    };
    let identity = ServerIdentity::generate("sculith-test-client", std::time::SystemTime::now())?;
    let signaling = HttpSignaling::new(uuid::Uuid::new_v4().to_string())?;
    let config = ConnectionConfig {
        identity: Some(std::sync::Arc::new(identity)),
        server_trust: Some(TokenTrust::Any),
        attempts: 1,
        ..Default::default()
    };
    let client = tokio::time::timeout(
        Duration::from_secs(10),
        NetherClient::connect_with(signaling, format!("http://{address}"), config),
    )
    .await??;
    let transport = TestNetherTransport {
        client,
        address,
        closed: false.into(),
    };
    let conn = Connection::<V2225, _>::from_transport_conn(transport);
    Ok(TestClient {
        conn,
        pending: VecDeque::new(),
    })
}

#[tokio::test]
async fn nethernet_http_webrtc_and_raknet_share_login_world_and_committed_block_changes() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .try_init();
    use crate::protocol::bedrock::nethernet::{ContextData, prepare};
    let server = Harness::start(Authentication::offline(), Duration::from_secs(15)).await;
    let directory = tempfile::tempdir().unwrap();
    let nether = prepare(
        "127.0.0.1:0".parse().unwrap(),
        &directory.path().join("identity.pem"),
    )
    .await
    .unwrap();
    let address = nether.local_addr().unwrap();
    let task = tokio::spawn(nether.serve(ContextData {
        authentication: Authentication::offline(),
        timeout: Duration::from_secs(10),
        world: server.world.clone(),
        status: server._status.subscribe(),
        shutdown: server.shutdown.subscribe(),
    }));
    let mut raknet = TestClient::connect(server.address).await.unwrap();
    let id = raknet.enter().await.unwrap();
    let mut nethernet = nether_client(address).await.unwrap();
    let other_id = nethernet.enter().await.unwrap();
    assert_ne!(id, other_id);
    raknet.request_break(id, 1, 3, 1).await.unwrap();
    for packet in [
        raknet.next().await.unwrap(),
        nethernet.next().await.unwrap(),
    ] {
        let V2225::UpdateBlockPacket(update) = packet else {
            panic!("expected shared block delta")
        };
        assert_eq!(
            (
                update.block_position.x,
                update.block_position.y,
                update.block_position.z
            ),
            (1, 3, 1)
        );
        assert_eq!(
            update.block_runtime_id,
            super::super::chunk::block_hash(sculith_world::BlockKind::Air).unwrap()
        );
    }
    assert_eq!(server.world.online_players(), 2);
    nethernet.conn.close().await;
    raknet.conn.close().await;
    server.shutdown.send_replace(true);
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server.stop().await;
    let _rebound = tokio::net::TcpListener::bind(address).await.unwrap();
}

#[tokio::test]
async fn independent_wire_client_completes_the_cookie_checked_open_handshake() {
    // Independent wire client: deliberately do not use the same RakNet codec
    // as the server. Preserve the advertised cookie challenge before the MTU.
    let server = Harness::start(Authentication::offline(), Duration::from_secs(10)).await;
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let magic = super::super::MAGIC;
    let mut first = vec![0x05];
    first.extend_from_slice(&magic);
    first.push(11);
    first.resize(548, 0);
    socket.send_to(&first, server.address).await.unwrap();
    let mut buffer = [0u8; 2048];
    let (size, _) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(buffer[0], 0x06);
    assert_eq!(&buffer[1..17], &magic);
    assert_eq!(buffer[25], 1);
    assert_eq!(size, 32);
    let cookie = [buffer[26], buffer[27], buffer[28], buffer[29]];
    let mtu = u16::from_be_bytes([buffer[30], buffer[31]]);
    let std::net::SocketAddr::V4(address) = server.address else {
        panic!("IPv4 fixture")
    };
    let mut second = vec![0x07];
    second.extend_from_slice(&magic);
    second.extend_from_slice(&cookie);
    second.push(0); // No libcat security challenge.
    second.push(4);
    second.extend(address.ip().octets().map(|octet| !octet));
    second.extend_from_slice(&address.port().to_be_bytes());
    second.extend_from_slice(&mtu.to_be_bytes());
    second.extend_from_slice(&12345u64.to_be_bytes());
    let mut invalid = second.clone();
    invalid[17] ^= 1;
    socket.send_to(&invalid, server.address).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), socket.recv_from(&mut buffer))
            .await
            .is_err()
    );
    socket.send_to(&second, server.address).await.unwrap();
    let (size, _) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(buffer[0], 0x08);
    assert_eq!(&buffer[1..17], &magic);
    assert_eq!(size, 35);
    assert_eq!(buffer[34], 0);
    server.stop().await;
}

#[tokio::test]
async fn crossing_a_chunk_boundary_keeps_the_negotiated_radius_and_sends_only_new_chunks() {
    let server = Harness::start(Authentication::offline(), Duration::from_secs(10)).await;
    let mut client = TestClient::connect(server.address).await.unwrap();
    client.encrypted_login().await.unwrap();
    client.finish_packs().await.unwrap();
    let id = client.receive_spawn_with_radius(1).await.unwrap();
    client
        .conn
        .send(&[V2225::SetLocalPlayerAsInitializedPacket(Box::new(
            SetLocalPlayerAsInitializedPacket {
                player_id: ActorRuntimeID(id),
            },
        ))])
        .await
        .unwrap();
    let mut movement = super::super::input::tests::movement();
    movement.player_position = (-0.5, 5.62, 0.5);
    movement.client_tick = 1;
    client
        .conn
        .send(&[V2225::PlayerAuthInputPacket(Box::new(movement))])
        .await
        .unwrap();
    let V2225::NetworkChunkPublisherUpdatePacket(publisher) = client.next().await.unwrap() else {
        panic!("expected view update after accepted chunk crossing");
    };
    assert_eq!(publisher.new_view_position.x, -1);
    assert_eq!(publisher.new_view_radius, 16);
    let mut positions = std::collections::BTreeSet::new();
    for _ in 0..3 {
        let V2225::LevelChunkPacket(chunk) = client.next().await.unwrap() else {
            panic!("expected newly visible chunk");
        };
        positions.insert((chunk.chunk_position.x, chunk.chunk_position.z));
    }
    assert_eq!(
        positions,
        [(-2, -1), (-2, 0), (-2, 1)].into_iter().collect()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), client.next())
            .await
            .is_err(),
        "overlapping or out-of-radius chunks must not be resent"
    );
    client.conn.close().await;
    server.stop().await;
}

#[tokio::test]
async fn two_encrypted_clients_share_committed_block_deltas_and_rejected_corrections() {
    use sculith_world::{BlockKind, ChunkPos};
    let server = Harness::start(Authentication::offline(), Duration::from_secs(10)).await;
    let mut a = TestClient::connect(server.address).await.unwrap();
    let id = a.enter().await.unwrap();
    let mut b = TestClient::connect(server.address).await.unwrap();
    b.enter().await.unwrap();
    a.request_break(id, 1, 3, 1).await.unwrap();
    for client in [&mut a, &mut b] {
        let V2225::UpdateBlockPacket(update) = client.next().await.unwrap() else {
            panic!("expected committed block delta")
        };
        assert_eq!(
            (
                update.block_position.x,
                update.block_position.y,
                update.block_position.z
            ),
            (1, 3, 1)
        );
        assert_eq!(
            update.block_runtime_id,
            super::super::chunk::block_hash(BlockKind::Air).unwrap()
        );
    }
    assert_eq!(
        server
            .world
            .snapshot(ChunkPos { x: 0, z: 0 })
            .await
            .unwrap()
            .get(1, 3, 1)
            .unwrap(),
        BlockKind::Air
    );
    a.request_break(id, 1, 0, 1).await.unwrap();
    let V2225::UpdateBlockPacket(update) = a.next().await.unwrap() else {
        panic!("expected protected block correction")
    };
    assert_eq!(
        update.block_runtime_id,
        super::super::chunk::block_hash(BlockKind::Bedrock).unwrap()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), b.conn.recv())
            .await
            .is_err(),
        "denials must not be broadcast as commits"
    );
    a.conn.close().await;
    b.conn.close().await;
    server.stop().await;
}

#[tokio::test]
async fn placement_uses_server_inventory_and_broadcasts_only_accepted_changes() {
    use bedrock::protocol::v662::types::InventoryTransactionData;
    use sculith_world::{BlockKind, ChunkPos};
    let server = Harness::start(Authentication::offline(), Duration::from_secs(10)).await;
    let mut a = TestClient::connect(server.address).await.unwrap();
    a.enter().await.unwrap();
    let mut b = TestClient::connect(server.address).await.unwrap();
    b.enter().await.unwrap();
    let mut forged = super::super::input::tests::transaction(0);
    if let V2225::InventoryTransactionPacket(transaction) = &mut forged
        && let InventoryTransactionData::ItemUse(data) = &mut transaction.data
    {
        data.held_item.id = 3;
    }
    a.conn.send(&[forged]).await.unwrap();
    let V2225::UpdateBlockPacket(correction) = a.next().await.unwrap() else {
        panic!("expected held-item correction")
    };
    assert_eq!(
        correction.block_runtime_id,
        super::super::chunk::block_hash(BlockKind::Air).unwrap()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), b.conn.recv())
            .await
            .is_err()
    );
    a.conn
        .send(&[super::super::input::tests::transaction(0)])
        .await
        .unwrap();
    for client in [&mut a, &mut b] {
        let V2225::UpdateBlockPacket(update) = client.next().await.unwrap() else {
            panic!("expected committed placement")
        };
        assert_eq!(
            (
                update.block_position.x,
                update.block_position.y,
                update.block_position.z
            ),
            (-1, 4, 0)
        );
        assert_eq!(
            update.block_runtime_id,
            super::super::chunk::block_hash(BlockKind::Stone).unwrap()
        );
    }
    assert_eq!(
        server
            .world
            .snapshot(ChunkPos { x: -1, z: 0 })
            .await
            .unwrap()
            .get(15, 4, 0)
            .unwrap(),
        BlockKind::Stone
    );
    a.conn.close().await;
    b.conn.close().await;
    server.stop().await;
}

#[tokio::test]
async fn valid_eye_offset_movement_is_committed_but_large_client_tick_cannot_teleport() {
    let server = Harness::start(Authentication::offline(), Duration::from_secs(10)).await;
    let mut client = TestClient::connect(server.address).await.unwrap();
    client.enter().await.unwrap();
    let mut events = server.world.subscribe();
    let mut request = super::super::input::tests::movement();
    request.player_position = (1.0, 5.62, 0.5);
    request.client_tick = 1;
    client
        .conn
        .send(&[V2225::PlayerAuthInputPacket(Box::new(request.clone()))])
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event.as_ref(), crate::world::WorldEvent::Moved { .. }) {
                break event;
            }
        }
    })
    .await
    .unwrap();
    let crate::world::WorldEvent::Moved { player, .. } = event.as_ref() else {
        unreachable!()
    };
    assert_eq!(player.feet.x, 1.0);
    assert_eq!(player.feet.y, 4.0);
    request.player_position = (20.0, 5.62, 0.5);
    request.client_tick = u64::MAX;
    client
        .conn
        .send(&[V2225::PlayerAuthInputPacket(Box::new(request))])
        .await
        .unwrap();
    let V2225::MovePlayerPacket(correction) = client.next().await.unwrap() else {
        panic!("expected position correction")
    };
    assert_eq!(correction.position, (1.0, 5.62, 0.5));
    client.conn.close().await;
    server.stop().await;
}

#[tokio::test]
async fn real_raknet_compression_encryption_and_ordered_pack_negotiation() {
    let server = Harness::start(Authentication::offline(), Duration::from_secs(1)).await;
    let mut client = TestClient::connect(server.address).await.unwrap();
    client.encrypted_login().await.unwrap();
    client.finish_packs().await.unwrap();
    let id = client.receive_spawn().await.unwrap();
    client
        .conn
        .send(&[V2225::SetLocalPlayerAsInitializedPacket(Box::new(
            SetLocalPlayerAsInitializedPacket {
                player_id: ActorRuntimeID(id),
            },
        ))])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while server.world.online_players() != 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    // Login timeout must never become a lifetime limit for initialized sessions.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(server.world.online_players(), 1);
    assert!(!client.conn.is_closed().await);
    client.conn.close().await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while server.world.online_players() != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    server.stop().await;
}

#[tokio::test]
async fn another_entity_id_cannot_be_acknowledged_as_the_local_player() {
    let server = Harness::start(Authentication::offline(), Duration::from_secs(10)).await;
    let mut client = TestClient::connect(server.address).await.unwrap();
    client.encrypted_login().await.unwrap();
    client.finish_packs().await.unwrap();
    let id = client.receive_spawn().await.unwrap();
    client
        .conn
        .send(&[V2225::SetLocalPlayerAsInitializedPacket(Box::new(
            SetLocalPlayerAsInitializedPacket {
                player_id: ActorRuntimeID(id + 1),
            },
        ))])
        .await
        .unwrap();
    assert!(matches!(
        client.next().await.unwrap(),
        V2225::DisconnectPacket(_)
    ));
    assert_eq!(server.world.online_players(), 0);
    client.conn.close().await;
    server.stop().await;
}

#[tokio::test]
async fn authenticated_mode_rejects_a_valid_self_signed_identity() {
    let authentication = Authentication {
        options: LoginOptions::default(),
        http: None,
    };
    let server = Harness::start(authentication, Duration::from_secs(10)).await;
    let mut client = TestClient::connect(server.address).await.unwrap();
    client.settings().await.unwrap();
    let identity = client.identity("NotAnOnlineAccount").unwrap();
    client.request_login(&identity).await.unwrap();
    assert!(matches!(
        client.next().await.unwrap(),
        V2225::DisconnectPacket(_)
    ));
    assert!(
        client.conn.encryption.is_none(),
        "unauthenticated identity must not reach encryption/success"
    );
    client.conn.close().await;
    server.stop().await;
}

#[tokio::test]
async fn obsolete_protocol_is_rejected_before_login() {
    let server = Harness::start(Authentication::offline(), Duration::from_secs(10)).await;
    let mut client = TestClient::connect(server.address).await.unwrap();
    client
        .conn
        .send(&[V2225::RequestNetworkSettingsPacket(Box::new(
            RequestNetworkSettingsPacket {
                client_network_version: 1,
            },
        ))])
        .await
        .unwrap();
    let V2225::PlayStatusPacket(status) = client.next().await.unwrap() else {
        panic!("expected old-client rejection");
    };
    assert!(matches!(status.status, PlayStatus::LoginFailedClientOld));
    client.conn.close().await;
    server.stop().await;
}

#[tokio::test]
async fn tampered_login_signature_is_rejected() {
    let server = Harness::start(Authentication::offline(), Duration::from_secs(10)).await;
    let mut client = TestClient::connect(server.address).await.unwrap();
    client.settings().await.unwrap();
    let identity = client.identity("SignatureTest").unwrap();
    let mut forged = identity.request().to_vec();
    let last = forged.last_mut().unwrap();
    *last = if *last == b'A' { b'B' } else { b'A' };
    client
        .conn
        .send(&[V2225::LoginPacket(Box::new(LoginPacket {
            client_network_version: V2225::PROTOCOL_VERSION as i32,
            connection_request: forged,
        }))])
        .await
        .unwrap();
    assert!(matches!(
        client.next().await.unwrap(),
        V2225::DisconnectPacket(_)
    ));
    client.conn.close().await;
    server.stop().await;
}

#[tokio::test]
async fn unfinished_login_times_out_and_releases_the_session() {
    let server = Harness::start(Authentication::offline(), Duration::from_millis(100)).await;
    let mut client = TestClient::connect(server.address).await.unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), client.conn.recv()).await,
        Ok(Err(_))
    ));
    client.conn.close().await;
    server.stop().await;
}

#[tokio::test]
async fn cancelled_pack_handshake_does_not_advance_to_world() {
    let server = Harness::start(Authentication::offline(), Duration::from_secs(10)).await;
    let mut client = TestClient::connect(server.address).await.unwrap();
    client.encrypted_login().await.unwrap();
    assert!(matches!(
        client.next().await.unwrap(),
        V2225::ResourcePacksInfoPacket(_)
    ));
    client
        .conn
        .send(&[V2225::ResourcePackClientResponsePacket(Box::new(
            ResourcePackClientResponsePacket::Cancel,
        ))])
        .await
        .unwrap();
    let V2225::DisconnectPacket(disconnect) = client.next().await.unwrap() else {
        panic!("cancelled packs must receive a disconnect, not a world spawn");
    };
    assert!(
        disconnect
            .message
            .unwrap()
            .kick_message
            .contains("negotiation failed")
    );
    assert_eq!(server.world.online_players(), 0);
    client.conn.close().await;
    server.stop().await;
}
