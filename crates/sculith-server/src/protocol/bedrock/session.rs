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
use super::{input::Intent, output, spawn};
use crate::world::WorldHandle;
use crate::{
    game::{BlockOutcome, Edition, MoveOutcome, Permissions, Position, Profile, Rotation},
    world::{PlayerLease, WorldEvent},
};

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub struct Authentication {
    options: LoginOptions,
    http: Option<ureq::Agent>,
}

/// Chosen by the accepted transport, never by a client packet or configuration.
pub enum Protection {
    PacketEncryption,
    WebRtcDtls(Arc<nethernet::identity::PlayerInfo>),
}

impl Protection {
    fn label(&self) -> &'static str {
        match self {
            Self::PacketEncryption => "bedrock-packet-cipher",
            Self::WebRtcDtls(_) => "webrtc-dtls",
        }
    }
}

impl Authentication {
    pub async fn new(required: bool) -> Result<Self> {
        if !required {
            return Ok(Self::offline());
        }
        tokio::task::spawn_blocking(|| {
            let http = crate::protocol::http_client::authentication_metadata();
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
    handle_with_protection(
        conn,
        authentication,
        world,
        timeout,
        Protection::PacketEncryption,
    )
    .await
}

pub async fn handle_with_protection<T: Transport>(
    conn: &mut Connection<V2225, T>,
    authentication: &Authentication,
    world: &WorldHandle,
    timeout: Duration,
    protection: Protection,
) -> Result<()> {
    let mut packets = Packets::new();
    let mut events = world.subscribe();
    let mut stage = "network-settings";
    let result = tokio::time::timeout(
        timeout,
        initialize(
            conn,
            &mut packets,
            authentication,
            world,
            &mut stage,
            &protection,
        ),
    )
    .await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(remote = %conn.get_socket_addr(), stage, "Bedrock login/spawn timed out");
            return Err(error).context(format!("Bedrock login/spawn timed out at {stage}"));
        }
    };
    let (lease, mut interest, radius) = match result {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(remote = %conn.get_socket_addr(), stage, failure = failure_kind(&error),
                "Bedrock login/spawn failed; use RUST_LOG=info,sculith::protocol::bedrock=debug for details");
            let _ = disconnect(
                conn,
                "Sculith: login verification or protocol negotiation failed.",
            )
            .await;
            return Err(error).with_context(|| format!("Bedrock initialization failed at {stage}"));
        }
    };
    let id = lease.player.id;
    let mut view_center = (0, 0);
    loop {
        tokio::select! {
            packet = packets.next(conn) => {
                let packet = packet?;
                let intents = match super::input::decode(&packet,id) {
                    Ok(intents)=>intents,
                    Err(error)=>{let _=disconnect(conn,"Sculith: invalid or unsupported gameplay request.").await;return Err(error);}
                };
                for intent in intents {
                    match intent {
                        Intent::Move {tick,position,rotation,head_yaw} => {
                            let outcome=world.move_player(id,tick,Position {x:f64::from(position.0),y:f64::from(position.1-spawn::EYE_OFFSET),z:f64::from(position.2)},
                                Rotation {pitch:f64::from(rotation.0),yaw:f64::from(rotation.1),head_yaw:f64::from(head_yaw)}).await?;
                            match outcome {
                                MoveOutcome::Rejected {correction:Some(player),reason}=>{
                                    tracing::debug!(?reason,"movement corrected");
                                    conn.send(&[output::movement(&player,tick,true)]).await?;
                                }
                                MoveOutcome::Accepted {player,..}=>{
                                    let center=((player.feet.x.floor() as i32).div_euclid(16),(player.feet.z.floor() as i32).div_euclid(16));
                                    if center!=view_center {spawn::refresh(conn,world,&mut interest,&player,radius).await?;view_center=center;}
                                }
                                _=>{},
                            }
                        }
                        Intent::Break {position}=>{
                            block_outcome(conn,world.break_block(id,position).await?).await?;
                        }
                        Intent::Place {against,face,hotbar_slot,claimed_item_network_id}=>{
                            let claimed=output::claimed_block(claimed_item_network_id)?;
                            block_outcome(conn,world.place_block(id,against,face,hotbar_slot,claimed).await?).await?;
                        }
                        Intent::SelectSlot {slot}=>{
                            let player=world.select_slot(id,slot).await?;
                            conn.send(&output::inventory(&player)?).await?;
                        }
                    }
                }
            }
            event = events.recv() => {
                let event=match event {
                    Ok(event)=>event,
                    Err(error)=>{let _=disconnect(conn,"Sculith: world event backlog; reconnect to resynchronize.").await;return Err(error.into());}
                };
                match event.as_ref() {
                    WorldEvent::Blocks {batch,revisions}=>{
                        for (position,revision) in revisions {
                            let Some(known)=interest.get(position).copied() else {continue};
                            if *revision<=known {continue;}
                            if *revision!=known+1 {
                                let snapshot=world.snapshot(*position).await?;
                                conn.send(&[V2225::LevelChunkPacket(Box::new(super::chunk::encode_chunk(&snapshot)?))]).await?;
                                interest.insert(*position,snapshot.revision());
                            } else {
                                let updates=batch.changes.iter().filter(|c|c.position.chunk()==*position)
                                    .map(|change|output::block_update(change.position,change.after)).collect::<Result<Vec<_>>>()?;
                                conn.send(&updates).await?;
                                interest.insert(*position,*revision);
                            }
                        }
                    }
                    WorldEvent::Joined(player)=>tracing::debug!(player=player.id,"shared world player joined"),
                    WorldEvent::Moved {player,tick}=>tracing::trace!(player=player.id,tick,"shared world player moved"),
                    WorldEvent::Left {id}=>tracing::debug!(player=id,"shared world player left"),
                }
            }
        }
    }
}

fn failure_kind(error: &anyhow::Error) -> &'static str {
    use bedrock::network::{
        error::{EncryptionError, NetworkCodecError},
        tokio::error::ConnectionError,
    };
    if error
        .downcast_ref::<nethernet::identity::error::IdentityError>()
        .is_some()
    {
        return "transport-identity";
    }
    if let Some(error) = error.downcast_ref::<ConnectionError>() {
        return match error {
            ConnectionError::NetworkCodecError(NetworkCodecError::EncryptionError(
                EncryptionError::InvalidTrailer,
            )) => "packet-cipher-integrity",
            ConnectionError::NetworkCodecError(NetworkCodecError::EncryptionError(
                EncryptionError::InvalidLength(_),
            )) => "packet-cipher-length",
            ConnectionError::NetworkCodecError(NetworkCodecError::CompressionError(_)) => {
                "compression-codec"
            }
            ConnectionError::NetworkCodecError(_) => "packet-codec",
            ConnectionError::ConnectionClosed | ConnectionError::TransportError(_) => {
                "transport-closed"
            }
            ConnectionError::IOError(_) => "connection-io",
        };
    }
    if error
        .downcast_ref::<bedrock::network::error::LoginError>()
        .is_some()
    {
        return "login-verification";
    }
    "protocol-state"
}

async fn block_outcome<T: Transport>(
    conn: &mut Connection<V2225, T>,
    outcome: BlockOutcome,
) -> Result<()> {
    if let BlockOutcome::Rejected {
        position,
        current: Some(block),
        reason,
    } = outcome
    {
        tracing::debug!(?reason, "block action corrected");
        conn.send(&[output::block_update(position, block)?]).await?;
    }
    Ok(())
}

async fn initialize<T: Transport>(
    conn: &mut Connection<V2225, T>,
    packets: &mut Packets,
    authentication: &Authentication,
    world: &WorldHandle,
    stage: &mut &'static str,
    protection: &Protection,
) -> Result<(PlayerLease, spawn::Interest, i32)> {
    let login = login(conn, packets, authentication, stage, protection).await?;
    tracing::info!(player = %login.authentication.identity().display_name,
        authenticated = login.authentication.is_authenticated(), protection = protection.label(), "Bedrock login completed");
    *stage = "resource-packs";
    empty_pack_handshake(conn, packets).await?;
    *stage = "world-spawn";
    let identity = login.authentication.identity();
    let subject = if login.authentication.is_authenticated() {
        format!("xuid:{}", identity.xuid)
    } else {
        format!("selfsigned:{}", identity.public_key)
    };
    let lease = world
        .reserve(
            Profile {
                subject,
                name: identity.display_name.clone(),
                uuid: uuid::Uuid::new_v4(),
                edition: Edition::Bedrock,
            },
            Permissions { can_build: true },
        )
        .await?;
    let entity_id = lease.player.id;
    conn.send(&spawn::initial_packets(&lease.player)?).await?;
    *stage = "chunk-request";
    let mut interest = None;
    loop {
        match packets.next(conn).await? {
            V2225::RequestChunkRadiusPacket(request) if interest.is_none() => {
                ensure!(request.chunk_radius > 0, "invalid requested chunk radius");
                let radius = request.chunk_radius.min(super::spawn::RADIUS);
                interest = Some((spawn::send_chunks(conn, world, radius).await?, radius));
                *stage = "spawn-acknowledgement";
            }
            V2225::SetLocalPlayerAsInitializedPacket(initialized) if interest.is_some() => {
                ensure!(
                    initialized.player_id.0 == entity_id,
                    "client initialized another entity ID"
                );
                tracing::info!(player = %login.authentication.identity().display_name, entity_id,
                    "Bedrock client acknowledged the flat-world spawn");
                world.activate(entity_id).await?;
                let (interest, radius) = interest.expect("checked above");
                return Ok((lease, interest, radius));
            }
            V2225::ServerBoundLoadingScreenPacket(_) | V2225::ClientCacheStatusPacket(_) => {}
            // Unordered movement may race the reliable initialization ack.
            // Pending players cannot mutate the owner; fresh movement is
            // accepted only after activation in the gameplay loop.
            V2225::PlayerAuthInputPacket(_) => {}
            _ => bail!("unexpected packet before world initialization"),
        }
    }
}

async fn login<T: Transport>(
    conn: &mut Connection<V2225, T>,
    packets: &mut Packets,
    authentication: &Authentication,
    stage: &mut &'static str,
    protection: &Protection,
) -> Result<Login> {
    let V2225::RequestNetworkSettingsPacket(request) = packets.next(conn).await? else {
        bail!("expected RequestNetworkSettings before login");
    };
    tracing::info!(remote = %conn.get_socket_addr(), client_protocol = request.client_network_version,
        expected_protocol = PROTOCOL, "Bedrock network settings requested");
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
    *stage = "login-request";
    let V2225::LoginPacket(request) = packets.next(conn).await? else {
        bail!("expected Login after network settings");
    };
    check_protocol(request.client_network_version, PROTOCOL)?;
    *stage = "identity-verification";
    let login = authentication.verify(request.connection_request).await?;
    if let Protection::WebRtcDtls(peer) = protection {
        *stage = "transport-identity-verification";
        let key = login.authentication.identity().public_key()?;
        peer.verify_login_key(&(&key).into())?;
        // DTLS already protects this connection. Vanilla NetherNet does not
        // run the RakNet application-cipher handshake on top of it.
        conn.send(&[status(PlayStatus::LoginSuccess)]).await?;
        return Ok(login);
    }
    packets.require_empty()?;
    *stage = "encryption-handshake";
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
