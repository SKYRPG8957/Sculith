//! Explicit latest-version login and pack state machine, with no discarded batch packets.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use bedrock::{
    auth::{AuthOIDC, Login},
    network::{
        compression::Compression,
        login::{LoginOptions, NetworkSettings, begin_encryption, check_protocol},
        tokio::{Connection, transport::Transport},
    },
    protocol::{
        ProtoVersion, V2225,
        v662::{
            enums::{PacketCompressionAlgorithm, PlayStatus},
            packets::{NetworkSettingsPacket, PlayStatusPacket, ServerToClientHandshakePacket},
            types::{BaseGameVersion, Experiments},
        },
        v712::packets::{DisconnectMessage, DisconnectPacket},
        v898::packets::ResourcePackStackPacket,
        v2168::packets::{ResourcePackClientResponsePacket, ResourcePacksInfoPacket},
        v2225::enums::ConnectionFailReason,
    },
};

const PROTOCOL: i32 = V2225::PROTOCOL_VERSION as i32;
const MAX_BATCH_PACKETS: usize = 64;
use crate::world::WorldHandle;

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub struct Authentication {
    options: LoginOptions,
    http: Option<ureq::Agent>,
}

impl Authentication {
    pub async fn new(required: bool) -> Result<Self> {
        if !required {
            return Ok(Self::offline());
        }
        tokio::task::spawn_blocking(|| {
            let http = bedrock::auth::http::ureq_agent();
            let oidc = AuthOIDC::fetch(&http).context(
                "cannot load Minecraft authentication public keys; check outbound access to the official discovery/OIDC endpoints"
            )?;
            Ok(Self { options: LoginOptions::default().oidc(Arc::new(oidc)), http: Some(http) })
        }).await.context("authentication initialization task failed")?
    }

    pub fn offline() -> Self {
        Self {
            options: LoginOptions::default().require_authentication(false),
            http: None,
        }
    }

    async fn verify(&self, request: Vec<u8>) -> Result<Login> {
        ensure!(
            request.len() <= super::transport::MAX_MESSAGE_BYTES,
            "connection request exceeds limit"
        );
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let login = match &this.http {
                Some(client) => this.options.verify_refreshing(&request, client)?,
                None => this.options.verify(&request)?,
            };
            let name = &login.authentication.identity().display_name;
            ensure!(
                !name.trim().is_empty() && name.len() <= 64 && !name.chars().any(char::is_control),
                "invalid player display name"
            );
            Ok(login)
        })
        .await
        .context("login verification task failed")?
    }
}

struct Packets {
    pending: VecDeque<V2225>,
}

impl Packets {
    fn new() -> Self {
        Self {
            pending: VecDeque::new(),
        }
    }

    async fn next<T: Transport>(&mut self, conn: &mut Connection<V2225, T>) -> Result<V2225> {
        if let Some(packet) = self.pending.pop_front() {
            return Ok(packet);
        }
        let packets = conn.recv_with_headers().await?;
        ensure!(
            !packets.is_empty() && packets.len() <= MAX_BATCH_PACKETS,
            "invalid login packet batch size"
        );
        for (header, packet) in packets {
            ensure!(
                header.sender_sub_client_id == 0 && header.target_sub_client_id == 0,
                "split-screen sub-clients are not implemented"
            );
            self.pending.push_back(packet);
        }
        self.pending.pop_front().context("empty packet batch")
    }

    fn require_empty(&self) -> Result<()> {
        ensure!(
            self.pending.is_empty(),
            "packets queued across a compression/encryption transition"
        );
        Ok(())
    }
}

fn status(status: PlayStatus) -> V2225 {
    V2225::PlayStatusPacket(Box::new(PlayStatusPacket { status }))
}

pub async fn disconnect<T: Transport>(
    conn: &mut Connection<V2225, T>,
    message: &str,
) -> Result<()> {
    conn.send(&[V2225::DisconnectPacket(Box::new(DisconnectPacket {
        reason: ConnectionFailReason::NoReason,
        message: Some(DisconnectMessage {
            kick_message: message.into(),
            filtered_message: String::new(),
        }),
    }))])
    .await?;
    Ok(())
}

pub async fn handle<T: Transport>(
    conn: &mut Connection<V2225, T>,
    authentication: &Authentication,
    world: &WorldHandle,
    timeout: Duration,
) -> Result<()> {
    let mut packets = Packets::new();
    let result = tokio::time::timeout(
        timeout,
        initialize(conn, &mut packets, authentication, world),
    )
    .await
    .context("Bedrock login/spawn timed out")?;
    match result {
        Ok(()) => {}
        Err(error) => {
            let _ = disconnect(
                conn,
                "Sculith: login verification or protocol negotiation failed.",
            )
            .await;
            return Err(error);
        }
    }
    let _lease = world.initialized_player();
    // This first spawn is read-only. The owner remains authoritative; no client
    // block/inventory/movement packet is applied as a world mutation yet.
    loop {
        let _packet = packets.next(conn).await?;
    }
}

async fn initialize<T: Transport>(
    conn: &mut Connection<V2225, T>,
    packets: &mut Packets,
    authentication: &Authentication,
    world: &WorldHandle,
) -> Result<()> {
    let login = login(conn, packets, authentication).await?;
    tracing::info!(player = %login.authentication.identity().display_name,
        authenticated = login.authentication.is_authenticated(), "Bedrock encrypted login completed");
    empty_pack_handshake(conn, packets).await?;
    let entity_id = world.entity_id().await?;
    conn.send(&super::spawn::initial_packets(entity_id)).await?;
    let mut chunks_sent = false;
    loop {
        match packets.next(conn).await? {
            V2225::RequestChunkRadiusPacket(request) if !chunks_sent => {
                ensure!(request.chunk_radius > 0, "invalid requested chunk radius");
                let radius = request.chunk_radius.min(super::spawn::RADIUS);
                super::spawn::send_chunks(conn, world, radius).await?;
                chunks_sent = true;
            }
            V2225::SetLocalPlayerAsInitializedPacket(initialized) if chunks_sent => {
                ensure!(
                    initialized.player_id.0 == entity_id,
                    "client initialized another entity ID"
                );
                tracing::info!(player = %login.authentication.identity().display_name, entity_id,
                    "Bedrock client acknowledged the flat-world spawn");
                return Ok(());
            }
            V2225::ServerBoundLoadingScreenPacket(_) | V2225::ClientCacheStatusPacket(_) => {}
            _ => bail!("unexpected packet before world initialization"),
        }
    }
}

async fn login<T: Transport>(
    conn: &mut Connection<V2225, T>,
    packets: &mut Packets,
    authentication: &Authentication,
) -> Result<Login> {
    let V2225::RequestNetworkSettingsPacket(request) = packets.next(conn).await? else {
        bail!("expected RequestNetworkSettings before login");
    };
    if let Err(error) = check_protocol(request.client_network_version, PROTOCOL) {
        conn.send(&[status(if request.client_network_version < PROTOCOL {
            PlayStatus::LoginFailedClientOld
        } else {
            PlayStatus::LoginFailedServerOld
        })])
        .await?;
        return Err(error.into());
    }
    packets.require_empty()?;
    let settings = NetworkSettings::default();
    let algorithm = match settings.compression {
        Compression::Zlib { .. } => PacketCompressionAlgorithm::ZLib,
        Compression::Snappy { .. } => PacketCompressionAlgorithm::Snappy,
        Compression::None => PacketCompressionAlgorithm::None,
    };
    conn.send(&[V2225::NetworkSettingsPacket(Box::new(
        NetworkSettingsPacket {
            compression_threshold: settings.threshold(),
            compression_algorithm: algorithm,
            client_throttle_enabled: false,
            client_throttle_threshold: 0,
            client_throttle_scalar: 0.0,
        },
    ))])
    .await?;
    conn.enable_compression(settings.compression);
    let V2225::LoginPacket(request) = packets.next(conn).await? else {
        bail!("expected Login after network settings");
    };
    check_protocol(request.client_network_version, PROTOCOL)?;
    packets.require_empty()?;
    let login = authentication.verify(request.connection_request).await?;
    let (token, encryption) = begin_encryption(&login)?;
    conn.send(&[V2225::ServerToClientHandshakePacket(Box::new(
        ServerToClientHandshakePacket {
            handshake_web_token: token,
        },
    ))])
    .await?;
    conn.enable_encryption(encryption);
    let V2225::ClientToServerHandshakePacket(_) = packets.next(conn).await? else {
        bail!("expected encrypted client handshake");
    };
    conn.send(&[status(PlayStatus::LoginSuccess)]).await?;
    Ok(login)
}

async fn empty_pack_handshake<T: Transport>(
    conn: &mut Connection<V2225, T>,
    packets: &mut Packets,
) -> Result<()> {
    conn.send(&[V2225::ResourcePacksInfoPacket(Box::new(
        ResourcePacksInfoPacket {
            resource_pack_required: false,
            has_addon_packs: false,
            has_scripts: false,
            force_disable_vibrant_visuals: false,
            world_template_uuid: uuid::Uuid::nil(),
            world_template_version: String::new(),
            resource_packs: vec![],
        },
    ))])
    .await?;
    let mut stack_sent = false;
    let mut cache_seen = false;
    loop {
        match packets.next(conn).await? {
            V2225::ClientCacheStatusPacket(_) if !cache_seen => {
                cache_seen = true;
            }
            V2225::ResourcePackClientResponsePacket(response) => match *response {
                ResourcePackClientResponsePacket::Cancel => {
                    bail!("client cancelled resource packs")
                }
                ResourcePackClientResponsePacket::DownloadingFinished if !stack_sent => {
                    conn.send(&[V2225::ResourcePackStackPacket(Box::new(
                        ResourcePackStackPacket {
                            texture_pack_required: false,
                            addon_list: vec![],
                            base_game_version: BaseGameVersion(V2225::GAME_VERSION.into()),
                            experiments: Experiments {
                                experiments: vec![],
                                ever_toggled: false,
                            },
                            include_editor_packs: false,
                        },
                    ))])
                    .await?;
                    stack_sent = true;
                }
                ResourcePackClientResponsePacket::ResourcePackStackFinished if stack_sent => {
                    return Ok(());
                }
                _ => bail!("resource-pack response out of sequence"),
            },
            _ => bail!("unexpected packet during resource-pack negotiation"),
        }
    }
}
