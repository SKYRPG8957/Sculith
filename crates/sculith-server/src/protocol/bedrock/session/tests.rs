use std::{collections::VecDeque, net::SocketAddr, time::Duration};

use anyhow::{Context, Result, ensure};
use bedrock::{
    auth::ClientData,
    network::{
        login::{ClientIdentity, LoginOptions, NetworkSettings},
        tokio::{Client, Connection},
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

struct TestClient {
    conn: Connection<V2225>,
    pending: VecDeque<V2225>,
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
        self.conn
            .send(&[V2225::RequestChunkRadiusPacket(Box::new(
                RequestChunkRadiusPacket {
                    chunk_radius: 8,
                    max_chunk_radius: 32,
                },
            ))])
            .await?;
        let V2225::ChunkRadiusUpdatedPacket(radius) = self.next().await? else {
            anyhow::bail!("expected radius acknowledgement");
        };
        ensure!(radius.chunk_radius == 2);
        ensure!(matches!(
            self.next().await?,
            V2225::NetworkChunkPublisherUpdatePacket(_)
        ));
        let mut chunks = std::collections::BTreeSet::new();
        for _ in 0..25 {
            let V2225::LevelChunkPacket(chunk) = self.next().await? else {
                anyhow::bail!("expected LevelChunk");
            };
            ensure!(chunk.dimension_id == 0 && chunk.sub_chunk_count == 24 && !chunk.cache_enabled);
            ensure!(chunk.serialized_chunk_data.len() == 1282);
            ensure!(
                (-2..=2).contains(&chunk.chunk_position.x)
                    && (-2..=2).contains(&chunk.chunk_position.z)
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
