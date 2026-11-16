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
        Self::start_with_content(authentication, timeout, None).await
    }

    async fn start_with_content(
        authentication: Authentication,
        timeout: Duration,
        content: Option<std::sync::Arc<crate::content::Content>>,
    ) -> Self {
        Self::start_observed(
            authentication,
            timeout,
            content,
            crate::connections::Observer::default(),
        )
        .await
    }

    async fn start_observed(
        authentication: Authentication,
        timeout: Duration,
        content: Option<std::sync::Arc<crate::content::Content>>,
        observer: crate::connections::Observer,
    ) -> Self {
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
                    super::Settings {
                        authentication,
                        timeout,
                        content,
                        observer,
                    },
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
    dtls_identity: Option<ClientIdentity>,
    own_id: u64,
}

impl<T: Transport> TestClient<T> {
    async fn enter(&mut self) -> Result<u64> {
        self.login().await?;
        self.finish_packs().await?;
        let id = self.receive_spawn().await?;
        self.conn
            .send(&[
                V2225::InteractPacket(Box::new(bedrock::protocol::v898::packets::InteractPacket {
                    action: bedrock::protocol::v898::packets::InteractPacketAction::InteractUpdate,
                    target_runtime_id: ActorRuntimeID(0),
                    mouse_position: None,
                })),
                V2225::SetLocalPlayerAsInitializedPacket(Box::new(
                    SetLocalPlayerAsInitializedPacket {
                        player_id: ActorRuntimeID(id),
                    },
                )),
            ])
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
        // Block/input fixtures ignore public presence, independently checked by
        // peer_smoke.py. Local movement corrections must remain observable.
        for _ in 0..256 {
            if self.pending.is_empty() {
                let batch = tokio::time::timeout(Duration::from_secs(3), self.conn.recv())
                    .await
                    .context("test receive timed out")??;
                ensure!(!batch.is_empty(), "empty test client batch");
                self.pending.extend(batch);
            }
            let packet = self.pending.pop_front().unwrap();
            match &packet {
                V2225::PlayerListPacket(_)
                | V2225::AddPlayerPacket(_)
                | V2225::RemoveActorPacket(_) => continue,
                V2225::MovePlayerPacket(p) if p.player_runtime_id.0 != self.own_id => continue,
                V2225::MobEquipmentPacket(p) if p.target_runtime_id.0 != self.own_id => continue,
                _ => return Ok(packet),
            }
        }
        anyhow::bail!("excessive public presence traffic in block fixture")
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

    async fn login(&mut self) -> Result<()> {
        self.settings().await?;
        let via_dtls = self.dtls_identity.is_some();
        let identity = match self.dtls_identity.take() {
            Some(identity) => identity,
            None => self.identity("SculithTests")?,
        };
        if via_dtls {
            self.conn
                .send(&[
                    V2225::LoginPacket(Box::new(LoginPacket {
                        client_network_version: V2225::PROTOCOL_VERSION as i32,
                        connection_request: identity.request().to_vec(),
                    })),
                    V2225::ClientCacheStatusPacket(Box::new(ClientCacheStatusPacket {
                        is_cache_supported: false,
                    })),
                ])
                .await?;
            let V2225::PlayStatusPacket(status) = self.next().await? else {
                anyhow::bail!(
                    "expected direct NetherNet login success, without an application-cipher handshake"
                );
            };
            ensure!(matches!(status.status, PlayStatus::LoginSuccess));
            ensure!(self.conn.encryption.is_none());
            return Ok(());
        }
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
        let V2225::JigsawStructureDataPacket(structure) = self.next().await? else {
            anyhow::bail!("required structure data must precede StartGame");
        };
        let nbtx::Value::Compound(registry) = structure.jigsaw_structure_data_tag else {
            anyhow::bail!("structure data must be a compound");
        };
        ensure!(registry.len() == 4);
        for name in ["processors", "template_pools", "jigsaws", "structure_sets"] {
            ensure!(matches!(
                registry.get(name.as_bytes()),
                Some(nbtx::Value::List(nbtx::ValueList::Compound(values))) if values.is_empty()
            ));
        }
        ensure!(matches!(self.next().await?, V2225::VoxelShapesPacket(_)));
        let V2225::StartGamePacket(game) = self.next().await? else {
            anyhow::bail!("expected StartGame");
        };
        ensure!(game.block_network_ids_are_hashes && game.position == (0.5, 5.62, 0.5));
        let id = game.target_runtime_id.0;
        self.own_id = id;
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
        for (id, size) in [(120, 5), (124, 57), (119, 1)] {
            let V2225::InventoryContentPacket(window) = self.next().await? else {
                anyhow::bail!("expected auxiliary inventory baseline");
            };
            ensure!(window.inventory_id == id && window.slots.len() == size);
            ensure!(
                window
                    .slots
                    .iter()
                    .all(|slot| slot.id == 0 && slot.net_id.is_none())
            );
        }
        let V2225::BiomeDefinitionListPacket(biomes) = self.next().await? else {
            anyhow::bail!("expected biome definitions");
        };
        ensure!(biomes.biomes.len() == 1 && biomes.strings[0] == "plains");
        ensure!(biomes.biomes[0].definition.id == u16::MAX);
        ensure!(matches!(
            self.next().await?,
            V2225::CreativeContentPacket(_)
        ));
        let V2225::AvailableCommandsPacket(commands) = self.next().await? else {
            anyhow::bail!("expected command registry");
        };
        ensure!(
            commands
                .commands
                .iter()
                .any(|command| command.name == "help")
        );
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
            dtls_identity: None,
            own_id: 0,
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
    let data = ClientData::offline(V2225::GAME_VERSION, &address.to_string(), "SculithTests");
    let dtls_identity =
        ClientIdentity::offline(ClientIdentity::generate_key(), "SculithTests", &data)?;
    let identity = ServerIdentity::from_key(
        dtls_identity.key().clone().into(),
        "sculith-test-client",
        std::time::SystemTime::now(),
        Some(Duration::from_secs(600)),
    )?;
    let signaling = HttpSignaling::new(uuid::Uuid::new_v4().to_string())?;
    tokio::time::timeout(
        Duration::from_secs(3),
        signaling.server_data(&format!("http://{address}")),
    )
    .await
    .context("NetherNet test client's HTTP status probe timed out")?
    .context("NetherNet test client's HTTP status probe failed")?;
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
        dtls_identity: Some(dtls_identity),
        own_id: 0,
    })
}

#[tokio::test]
async fn authenticated_nethernet_crosses_nonloopback_http_fixed_udp_and_oidc_before_spawning() {
    use crate::protocol::bedrock::nethernet::{ContextData, PublicMedia, test_support};
    let Some(ip) = if_addrs::get_if_addrs()
        .unwrap()
        .into_iter()
        .map(|interface| interface.ip())
        .find(|ip| ip.is_ipv4() && !ip.is_loopback() && !ip.is_unspecified())
    else {
        eprintln!("SKIP: public NetherNet fixture requires a non-loopback interface");
        return;
    };
    let issuer = test_support::TestIssuer::new("online-gameplay-fixture");
    let authentication = Authentication {
        options: LoginOptions::default().oidc(std::sync::Arc::new(issuer.oidc())),
        http: Some(crate::protocol::http_client::authentication_metadata()),
    };
    let udp = UdpSocket::bind(SocketAddr::new(ip, 0)).await.unwrap();
    let forbidden = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    // Hold a small free range while setting up the server. The client is already
    // bound, so it cannot acquire one of the advertised server ports.
    let mut reservation = None;
    for _ in 0..32 {
        let first = UdpSocket::bind(SocketAddr::new(ip, 0)).await.unwrap();
        let port = first.local_addr().unwrap().port();
        if port > 65532 {
            continue;
        }
        let mut held = vec![first];
        for next in port + 1..=port + 3 {
            if let Ok(socket) = UdpSocket::bind(SocketAddr::new(ip, next)).await {
                held.push(socket);
            } else {
                break;
            }
        }
        if held.len() == 4 {
            reservation = Some(([port, port + 3], held));
            break;
        }
    }
    let (ports, guards) = reservation.expect("no small free UDP range for public fixture");
    let media = PublicMedia {
        ports,
        advertised_ip: Some(ip),
    };
    let directory = tempfile::tempdir().unwrap();
    let (server, operator) = test_support::server(
        SocketAddr::new(ip, 0),
        &directory.path().join("identity.pem"),
        Some(&media),
        &issuer,
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    let (stop, shutdown) = watch::channel(false);
    let (world, owner) = crate::world::create(shutdown.clone());
    let owner = tokio::spawn(owner);
    let (_status_sender, status) =
        watch::channel(ServerStatus::new("authenticated public fixture".into(), 1).unwrap());
    let task = tokio::spawn(server.serve(ContextData {
        settings: super::Settings {
            authentication,
            timeout: Duration::from_secs(15),
            content: None,
            observer: crate::connections::Observer::default(),
        },
        world: world.clone(),
        status,
        shutdown: shutdown.clone(),
    }));
    let key = ClientIdentity::generate_key();
    let signer = nethernet::identity::ServerIdentity::from_key(
        key.clone().into(),
        "fixture.client",
        std::time::SystemTime::now(),
        Some(Duration::from_secs(600)),
    )
    .unwrap();
    let token=issuer.token(signer.token(),serde_json::json!({"mid":"fixture-mid","xid":"2535400000000001","xname":"PublicNetherTest"}));
    let data = ClientData::offline(
        V2225::GAME_VERSION,
        &address.to_string(),
        "PublicNetherTest",
    );
    let identity = ClientIdentity::online(
        key,
        bedrock::auth::AuthData {
            auth_type: bedrock::auth::AuthType::Online,
            auth_payload: bedrock::auth::AuthPayload::Token(token),
        },
        &data,
    )
    .unwrap();
    let blocked_udp = UdpSocket::bind(SocketAddr::new(ip, 0)).await.unwrap();
    let unavailable = test_support::connect(
        address,
        blocked_udp,
        &signer,
        &operator,
        &issuer,
        shutdown.clone(),
        None,
    )
    .await;
    match unavailable {
        Err(error) => assert!(error.to_string().contains("HTTP/1.1 503"), "{error:#}"),
        Ok(_) => panic!("a full UDP range accepted another media connection"),
    }
    assert_eq!(world.online_players(), 0);
    drop(guards);
    let (transport, client_task) = test_support::connect(
        address,
        udp,
        &signer,
        &operator,
        &issuer,
        shutdown,
        Some(forbidden.local_addr().unwrap()),
    )
    .await
    .unwrap();
    let mut client = TestClient {
        conn: Connection::<V2225, _>::from_transport_conn(transport),
        pending: VecDeque::new(),
        dtls_identity: Some(identity),
        own_id: 0,
    };
    let id = tokio::time::timeout(Duration::from_secs(15), client.enter())
        .await
        .unwrap()
        .unwrap();
    assert!(!client.conn.get_socket_addr().ip().is_loopback());
    assert!((ports[0]..=ports[1]).contains(&client.conn.get_socket_addr().port()));
    tokio::time::timeout(Duration::from_secs(3), async {
        while world.online_players() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let player = world.player_snapshot(id).await.unwrap();
    assert_eq!(player.profile.subject, "xuid:2535400000000001");
    assert_eq!(player.profile.name, "PublicNetherTest");
    assert_eq!(world.online_players(), 1);
    let mut packet = [0u8; 128];
    assert_eq!(
        forbidden.try_recv_from(&mut packet).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "an untrusted SDP candidate received a UDP connectivity check"
    );
    assert!(
        client.conn.encryption.is_none(),
        "DTLS must not add a RakNet packet cipher"
    );
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), client_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    owner.await.unwrap().unwrap();
    assert_eq!(world.online_players(), 0);
    for port in ports[0]..=ports[1] {
        UdpSocket::bind(SocketAddr::new(ip, port)).await.unwrap();
    }
    eprintln!(
        "PUBLIC_AUTH_NETHERNET_SPAWN: fixture-issued RSA account, P-384 client data, pinned operator and fixed UDP ports"
    );
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
        &Authentication::offline(),
        None,
    )
    .await
    .unwrap();
    let address = nether.local_addr().unwrap();
    let task = tokio::spawn(nether.serve(ContextData {
        settings: crate::protocol::bedrock::session::Settings {
            authentication: Authentication::offline(),
            timeout: Duration::from_secs(10),
            content: None,
            observer: crate::connections::Observer::default(),
        },
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
    let mut events = server.world.subscribe();
    let mut movement = super::super::input::tests::movement();
    movement.player_position = (1.0, 5.62, 0.5);
    movement.client_tick = 1;
    let encoded = bedrock::network::codec::encode_packets(
        &[V2225::PlayerAuthInputPacket(Box::new(movement.clone()))],
        nethernet.conn.compression.as_ref(),
        None,
    )
    .unwrap();
    nethernet
        .conn
        .get_transport_conn()
        .client
        .send_unreliable(bytes::Bytes::from(encoded))
        .await
        .unwrap();
    let moved = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.unwrap();
            if let crate::world::WorldEvent::Moved { player, .. } = event.as_ref()
                && player.id == other_id
            {
                break player.clone();
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(moved.feet.x, 1.0);
    assert_eq!(moved.feet.y, 4.0);
    movement.player_position = (20.0, 5.62, 0.5);
    movement.client_tick = u64::MAX;
    let encoded = bedrock::network::codec::encode_packets(
        &[V2225::PlayerAuthInputPacket(Box::new(movement))],
        nethernet.conn.compression.as_ref(),
        None,
    )
    .unwrap();
    nethernet
        .conn
        .get_transport_conn()
        .client
        .send_unreliable(bytes::Bytes::from(encoded))
        .await
        .unwrap();
    let V2225::MovePlayerPacket(correction) = nethernet.next().await.unwrap() else {
        panic!("expected reliable correction of an invalid unreliable movement")
    };
    assert_eq!(correction.position, (1.0, 5.62, 0.5));
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
async fn nethernet_rejects_a_login_not_bound_to_its_signed_transport_key() {
    use crate::protocol::bedrock::nethernet::{ContextData, prepare};
    let server = Harness::start(Authentication::offline(), Duration::from_secs(15)).await;
    let directory = tempfile::tempdir().unwrap();
    let nether = prepare(
        "127.0.0.1:0".parse().unwrap(),
        &directory.path().join("identity.pem"),
        &Authentication::offline(),
        None,
    )
    .await
    .unwrap();
    let address = nether.local_addr().unwrap();
    let task = tokio::spawn(nether.serve(ContextData {
        settings: crate::protocol::bedrock::session::Settings {
            authentication: Authentication::offline(),
            timeout: Duration::from_secs(10),
            content: None,
            observer: crate::connections::Observer::default(),
        },
        world: server.world.clone(),
        status: server._status.subscribe(),
        shutdown: server.shutdown.subscribe(),
    }));
    let mut client = nether_client(address).await.unwrap();
    client.settings().await.unwrap();
    // Both signatures are valid, but this login is for a different key than
    // the one that signed the DTLS fingerprint assertion.
    let unrelated = client.identity("ReplayedIdentity").unwrap();
    client.request_login(&unrelated).await.unwrap();
    assert!(matches!(
        client.next().await.unwrap(),
        V2225::DisconnectPacket(_)
    ));
    assert_eq!(server.world.online_players(), 0);
    client.conn.close().await;
    server.shutdown.send_replace(true);
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server.stop().await;
}

#[tokio::test]
async fn crossing_a_chunk_boundary_keeps_the_negotiated_radius_and_sends_only_new_chunks() {
    let server = Harness::start(Authentication::offline(), Duration::from_secs(10)).await;
    let mut client = TestClient::connect(server.address).await.unwrap();
    client.login().await.unwrap();
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
        tokio::time::timeout(Duration::from_millis(100), b.next())
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
        tokio::time::timeout(Duration::from_millis(100), b.next())
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
async fn loading_controls_and_radius_renegotiation_do_not_disconnect_or_mutate_pending_players() {
    use bedrock::protocol::v662::packets::{EmoteListPacket, NetworkStackLatencyPacket};
    let server = Harness::start(Authentication::offline(), Duration::from_secs(10)).await;
    let mut client = TestClient::connect(server.address).await.unwrap();
    client.login().await.unwrap();
    client.finish_packs().await.unwrap();
    let id = client.receive_spawn().await.unwrap();
    // This completion request is not allowed to mutate a pending player.
    client.request_break(id, 1, 3, 1).await.unwrap();
    client
        .conn
        .send(&[
            V2225::InteractPacket(Box::new(bedrock::protocol::v898::packets::InteractPacket {
                action: bedrock::protocol::v898::packets::InteractPacketAction::InteractUpdate,
                target_runtime_id: ActorRuntimeID(0),
                mouse_position: None,
            })),
            V2225::NetworkStackLatencyPacket(Box::new(NetworkStackLatencyPacket {
                creation_time: 123456,
                is_from_server: true,
            })),
            V2225::EmoteListPacket(Box::new(EmoteListPacket {
                runtime_id: ActorRuntimeID(id),
                emote_piece_ids: vec![uuid::Uuid::new_v4()],
            })),
            V2225::RequestChunkRadiusPacket(Box::new(RequestChunkRadiusPacket {
                chunk_radius: 32,
                max_chunk_radius: 32,
            })),
        ])
        .await
        .unwrap();
    let V2225::NetworkStackLatencyPacket(echo) = client.next().await.unwrap() else {
        panic!("expected latency acknowledgement during loading");
    };
    assert_eq!(echo.creation_time, 123456);
    assert!(!echo.is_from_server);
    let V2225::ChunkRadiusUpdatedPacket(radius) = client.next().await.unwrap() else {
        panic!("expected repeated radius acknowledgement without duplicated chunks");
    };
    assert_eq!(radius.chunk_radius, 2);
    assert_eq!(server.world.online_players(), 0);
    assert_eq!(server.world.capture().await.unwrap().revision(), 0);
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
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    client.request_break(id, 1, 3, 1).await.unwrap();
    assert!(matches!(
        client.next().await.unwrap(),
        V2225::UpdateBlockPacket(_)
    ));
    assert_eq!(server.world.capture().await.unwrap().revision(), 1);
    // Renegotiation and control packets also remain live after initialization.
    client
        .conn
        .send(&[V2225::RequestChunkRadiusPacket(Box::new(
            RequestChunkRadiusPacket {
                chunk_radius: 1,
                max_chunk_radius: 32,
            },
        ))])
        .await
        .unwrap();
    let V2225::ChunkRadiusUpdatedPacket(radius) = client.next().await.unwrap() else {
        panic!("expected live radius acknowledgement");
    };
    assert_eq!(radius.chunk_radius, 1);
    assert!(matches!(
        client.next().await.unwrap(),
        V2225::NetworkChunkPublisherUpdatePacket(_)
    ));
    client
        .conn
        .send(&[V2225::NetworkStackLatencyPacket(Box::new(
            NetworkStackLatencyPacket {
                creation_time: 789,
                is_from_server: true,
            },
        ))])
        .await
        .unwrap();
    let V2225::NetworkStackLatencyPacket(echo) = client.next().await.unwrap() else {
        panic!("expected live latency acknowledgement");
    };
    assert_eq!(echo.creation_time, 789);
    client.conn.close().await;
    server.stop().await;
}

#[tokio::test]
async fn loading_notifications_cannot_restart_network_or_login_negotiation() {
    let server = Harness::start(Authentication::offline(), Duration::from_secs(10)).await;
    let mut client = TestClient::connect(server.address).await.unwrap();
    client.login().await.unwrap();
    client.finish_packs().await.unwrap();
    client.receive_spawn().await.unwrap();
    client
        .conn
        .send(&[V2225::RequestNetworkSettingsPacket(Box::new(
            RequestNetworkSettingsPacket {
                client_network_version: V2225::PROTOCOL_VERSION as i32,
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
async fn java_and_both_bedrock_transports_share_bidirectional_commits_in_one_owner() {
    use crate::protocol::java::{self, Preview, test_client::Probe};
    use sculith_world::{BlockKind, BlockPos};
    let server = Harness::start(Authentication::offline(), Duration::from_secs(15)).await;
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = tcp.local_addr().unwrap();
    let (stop, shutdown) = watch::channel(false);
    let java = tokio::spawn(java::serve(
        tcp,
        server._status.subscribe(),
        Some(Preview {
            authentication: None,
            content: None,
            world: server.world.clone(),
            timeout: Duration::from_secs(30),
            shutdown: shutdown.clone(),
        }),
        shutdown,
        crate::connections::Observer::default(),
    ));
    let mut bedrock = TestClient::connect(server.address).await.unwrap();
    let bedrock_id = bedrock.enter().await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let nether = crate::protocol::bedrock::nethernet::prepare(
        "127.0.0.1:0".parse().unwrap(),
        &directory.path().join("identity.pem"),
        &Authentication::offline(),
        None,
    )
    .await
    .unwrap();
    let nether_address = nether.local_addr().unwrap();
    let nether_task = tokio::spawn(nether.serve(
        crate::protocol::bedrock::nethernet::ContextData {
            settings: crate::protocol::bedrock::session::Settings {
                authentication: Authentication::offline(),
                timeout: Duration::from_secs(15),
                content: None,
                observer: crate::connections::Observer::default(),
            },
            world: server.world.clone(),
            status: server._status.subscribe(),
            shutdown: server.shutdown.subscribe(),
        },
    ));
    let mut nethernet = nether_client(nether_address).await.unwrap();
    nethernet.enter().await.unwrap();
    let mut java_client = Probe::enter(address).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.world.online_players() != 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let target = BlockPos { x: 1, y: 3, z: 1 };
    bedrock.request_break(bedrock_id, 1, 3, 1).await.unwrap();
    assert!(matches!(
        bedrock.next().await.unwrap(),
        V2225::UpdateBlockPacket(_)
    ));
    assert!(matches!(
        nethernet.next().await.unwrap(),
        V2225::UpdateBlockPacket(_)
    ));
    assert_eq!(java_client.next_block().await.unwrap(), (target, 0));
    java_client
        .place_stone(BlockPos { x: 1, y: 2, z: 1 }, 1)
        .await
        .unwrap();
    assert_eq!(java_client.next_block().await.unwrap(), (target, 1));
    let V2225::UpdateBlockPacket(update) = bedrock.next().await.unwrap() else {
        panic!("Bedrock did not receive the Java edit");
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
        super::super::chunk::block_hash(BlockKind::Stone).unwrap()
    );
    let V2225::UpdateBlockPacket(update) = nethernet.next().await.unwrap() else {
        panic!("NetherNet did not receive the Java edit");
    };
    assert_eq!(
        update.block_runtime_id,
        super::super::chunk::block_hash(BlockKind::Stone).unwrap()
    );
    assert_eq!(server.world.capture().await.unwrap().revision(), 2);
    assert_eq!(
        server
            .world
            .snapshot(target.chunk())
            .await
            .unwrap()
            .get(1, 3, 1)
            .unwrap(),
        BlockKind::Stone
    );
    stop.send_replace(true);
    tokio::time::timeout(Duration::from_secs(2), java)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while server.world.online_players() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    bedrock.conn.close().await;
    nethernet.conn.close().await;
    server.shutdown.send_replace(true);
    tokio::time::timeout(Duration::from_secs(2), nether_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server.stop().await;
}

#[tokio::test]
async fn required_pack_rejects_unknown_ids_early_finish_invalid_chunks_and_excess_retries() {
    use bedrock::protocol::v662::packets::ResourcePackChunkRequestPacket;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/pack");
    let bundle =
        sculith_packs::bundle::Bundle::prepare(&root.join("bedrock"), &root.join("textures.json"))
            .unwrap();
    let delivery = crate::content::Delivery::prepare("127.0.0.1:0".parse().unwrap(), None, bundle)
        .await
        .unwrap();
    for case in 0..4 {
        let server = Harness::start_with_content(
            Authentication::offline(),
            Duration::from_secs(15),
            Some(delivery.content.clone()),
        )
        .await;
        let mut client = TestClient::connect(server.address).await.unwrap();
        client.login().await.unwrap();
        let V2225::ResourcePacksInfoPacket(info) = client.next().await.unwrap() else {
            panic!("expected pack info");
        };
        assert!(info.resource_pack_required);
        assert_eq!(info.resource_packs.len(), 1);
        let pack = &info.resource_packs[0];
        let name = format!("{}_{}", pack.id, pack.version);
        let requested = if case == 0 {
            "unadvertised-pack".to_string()
        } else {
            name.clone()
        };
        client
            .conn
            .send(&[V2225::ResourcePackClientResponsePacket(Box::new(
                ResourcePackClientResponsePacket::Downloading(vec![requested]),
            ))])
            .await
            .unwrap();
        if case != 0 {
            assert!(matches!(
                client.next().await.unwrap(),
                V2225::ResourcePackDataInfoPacket(_)
            ));
            if case == 1 {
                client
                    .conn
                    .send(&[V2225::ResourcePackClientResponsePacket(Box::new(
                        ResourcePackClientResponsePacket::DownloadingFinished,
                    ))])
                    .await
                    .unwrap();
            } else {
                for attempt in 0..if case == 2 { 1 } else { 3 } {
                    client
                        .conn
                        .send(&[V2225::ResourcePackChunkRequestPacket(Box::new(
                            ResourcePackChunkRequestPacket {
                                resource_name: name.clone(),
                                chunk: if case == 2 { u32::MAX } else { 0 },
                            },
                        ))])
                        .await
                        .unwrap();
                    if case == 3 && attempt < 2 {
                        assert!(matches!(
                            client.next().await.unwrap(),
                            V2225::ResourcePackChunkDataPacket(_)
                        ));
                    }
                }
            }
        }
        assert!(
            matches!(client.next().await.unwrap(), V2225::DisconnectPacket(_)),
            "case {case}"
        );
        assert_eq!(server.world.online_players(), 0);
        client.conn.close().await;
        server.stop().await;
    }
}

#[tokio::test]
async fn rejected_batch_prefix_does_not_hide_decoded_trailing_packet_receipts() {
    use sculith_core::connections::{ConnectionEvent, PacketPhase};
    let observer = crate::connections::Observer::default();
    let mut events = observer.subscribe();
    observer.configure(true);
    let server = Harness::start_observed(
        Authentication::offline(),
        Duration::from_secs(5),
        None,
        observer,
    )
    .await;
    let mut client = TestClient::connect(server.address).await.unwrap();
    // The second packet is invalid here: compression has not been negotiated.
    // Both decoded identifiers must be observable, but neither may advance login.
    client
        .conn
        .send(&[
            V2225::RequestNetworkSettingsPacket(Box::new(RequestNetworkSettingsPacket {
                client_network_version: V2225::PROTOCOL_VERSION as i32,
            })),
            V2225::ClientToServerHandshakePacket(Box::new(ClientToServerHandshakePacket {})),
        ])
        .await
        .unwrap();
    assert!(matches!(
        client.next().await.unwrap(),
        V2225::DisconnectPacket(_)
    ));
    assert_eq!(server.world.online_players(), 0);
    client.conn.close().await;
    server.stop().await;
    let mut receipts = Vec::new();
    let mut closes = 0;
    while let Ok(event) = events.try_recv() {
        match event {
            ConnectionEvent::PacketReceived { packet, .. } => receipts.push(packet),
            ConnectionEvent::PlayerAttached { .. } => panic!("rejected batch activated a player"),
            ConnectionEvent::Closed { .. } => closes += 1,
            _ => {}
        }
    }
    assert_eq!(closes, 1);
    assert_eq!(receipts.len(), 2);
    assert_eq!(
        receipts.iter().map(|p| p.sequence).collect::<Vec<_>>(),
        [1, 2]
    );
    assert!(
        receipts
            .iter()
            .all(|p| p.phase == PacketPhase::NetworkSettings)
    );
    assert_ne!(receipts[0].id, receipts[1].id);
}

#[tokio::test]
async fn real_raknet_compression_encryption_and_ordered_pack_negotiation() {
    use sculith_core::connections::{ConnectionEvent, ConnectionTransport, PacketPhase};
    let observer = crate::connections::Observer::default();
    let mut events = observer.subscribe();
    observer.configure(true);
    let server = Harness::start_observed(
        Authentication::offline(),
        Duration::from_secs(1),
        None,
        observer,
    )
    .await;
    let mut client = TestClient::connect(server.address).await.unwrap();
    client.login().await.unwrap();
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
    client
        .conn
        .send(&[V2225::ClientCacheStatusPacket(Box::new(
            ClientCacheStatusPacket {
                is_cache_supported: false,
            },
        ))])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    client.conn.close().await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while server.world.online_players() != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    server.stop().await;
    let mut sequence = 0;
    let mut phases = Vec::new();
    let mut attached = 0;
    let mut closed = 0;
    assert!(matches!(
        events.try_recv().unwrap(),
        ConnectionEvent::Opened {
            connection_id: 1,
            transport: ConnectionTransport::BedrockRaknet,
        }
    ));
    while let Ok(event) = events.try_recv() {
        match event {
            ConnectionEvent::PacketReceived {
                connection_id,
                packet,
            } => {
                assert_eq!(connection_id, 1);
                assert_eq!(packet.sequence, sequence + 1);
                sequence = packet.sequence;
                phases.push(packet.phase);
            }
            ConnectionEvent::PlayerAttached {
                connection_id,
                player_id,
            } => {
                assert_eq!((connection_id, player_id), (1, id));
                attached += 1;
            }
            ConnectionEvent::Closed { connection_id } => {
                assert_eq!(connection_id, 1);
                closed += 1;
            }
            _ => panic!("unexpected extra connection"),
        }
    }
    assert_eq!((attached, closed), (1, 1));
    for phase in [
        PacketPhase::NetworkSettings,
        PacketPhase::Login,
        PacketPhase::Encryption,
        PacketPhase::ResourcePacks,
        PacketPhase::Spawn,
        PacketPhase::Play,
    ] {
        assert!(phases.contains(&phase), "missing phase {phase:?}");
    }
}

#[tokio::test]
async fn another_entity_id_cannot_be_acknowledged_as_the_local_player() {
    let server = Harness::start(Authentication::offline(), Duration::from_secs(10)).await;
    let mut client = TestClient::connect(server.address).await.unwrap();
    client.login().await.unwrap();
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
    client.login().await.unwrap();
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
