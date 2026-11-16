//! Explicit latest-version login and pack state machine, with no discarded batch packets.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use bedrock::protocol::Packets as _;
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
        },
        v712::packets::{DisconnectMessage, DisconnectPacket},
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

mod connection;
mod packs;
pub(super) use connection::Observed;
#[cfg(test)]
mod tests;

#[derive(Clone)]
pub struct Settings {
    pub authentication: Authentication,
    pub timeout: Duration,
    pub content: Option<Arc<crate::content::Content>>,
    pub observer: crate::connections::Observer,
}

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
    pub(super) fn is_required(&self) -> bool {
        self.http.is_some()
    }

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
    trace: crate::connections::Trace,
    phase: sculith_core::connections::PacketPhase,
}

impl Packets {
    fn new(observer: &crate::connections::Observer, protection: &Protection) -> Self {
        use sculith_core::connections::{ConnectionTransport, PacketPhase};
        let trace = observer.open(match protection {
            Protection::PacketEncryption => ConnectionTransport::BedrockRaknet,
            Protection::WebRtcDtls(_) => ConnectionTransport::BedrockNethernet,
        });
        trace.select_protocol(PROTOCOL as u32);
        trace.set_phase(PacketPhase::NetworkSettings);
        Self {
            pending: VecDeque::new(),
            trace,
            phase: PacketPhase::NetworkSettings,
        }
    }

    async fn next<T: Transport>(&mut self, conn: &mut Observed<'_, T>) -> Result<V2225> {
        if let Some(packet) = self.pending.pop_front() {
            return Ok(packet);
        }
        let packets = conn.recv_with_headers().await?;
        if packets.is_empty() || packets.len() > MAX_BATCH_PACKETS {
            tracing::warn!(
                packet_count = packets.len(),
                limit = MAX_BATCH_PACKETS,
                "Bedrock packet batch count out of bounds"
            );
            bail!("invalid login packet batch size");
        }
        // Observe the complete bounded batch before interpreting any member.
        // A rejected prefix must not hide already decoded trailing packets.
        // Phase is the ingress state, even if a prefix later advances it.
        if self.trace.enabled() {
            for (_, packet) in &packets {
                let info = super::catalog::receipt(packet, 1);
                self.trace
                    .packet(sculith_core::connections::ConnectionPacket {
                        protocol: Some(info.protocol),
                        phase: self.phase,
                        id: info.id,
                        name: info.name,
                        dispatch: info.dispatch,
                        sequence: 0,
                    });
            }
        }
        for (header, packet) in packets {
            if header.sender_sub_client_id != 0 || header.target_sub_client_id != 0 {
                tracing::warn!(
                    sender_sub_client = header.sender_sub_client_id,
                    target_sub_client = header.target_sub_client_id,
                    "Bedrock split-screen sub-client is unsupported"
                );
                bail!("split-screen sub-clients are not implemented");
            }
            self.pending.push_back(packet);
        }
        self.pending.pop_front().context("empty packet batch")
    }

    fn set_phase(&mut self, phase: sculith_core::connections::PacketPhase) {
        self.phase = phase;
        self.trace.set_phase(phase);
    }

    fn require_empty(&self) -> Result<()> {
        if !self.pending.is_empty() {
            tracing::warn!(
                pending_packets = self.pending.len(),
                "Bedrock packets queued across a codec transition"
            );
            bail!("packets queued across a compression/encryption transition");
        }
        Ok(())
    }
}

fn status(status: PlayStatus) -> V2225 {
    V2225::PlayStatusPacket(Box::new(PlayStatusPacket { status }))
}

async fn disconnect<T: Transport>(conn: &mut Observed<'_, T>, message: &str) -> Result<()> {
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
    settings: &Settings,
    world: &WorldHandle,
) -> Result<()> {
    handle_with_protection(conn, settings, world, Protection::PacketEncryption).await
}

pub async fn handle_with_protection<T: Transport>(
    conn: &mut Connection<V2225, T>,
    settings: &Settings,
    world: &WorldHandle,
    protection: Protection,
) -> Result<()> {
    let mut packets = Packets::new(&settings.observer, &protection);
    let mut observed = Observed::new(conn, packets.trace.clone());
    let conn = &mut observed;
    let mut events = world.subscribe();
    let mut stage = "network-settings";
    let result = tokio::time::timeout(
        settings.timeout,
        initialize(
            conn,
            &mut packets,
            &settings.authentication,
            world,
            &mut stage,
            &protection,
            settings.content.as_deref(),
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
    let (lease, mut interest, mut radius) = match result {
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
    let observation = world.observe_players(id).await?;
    let mut peer_views = sculith_runtime::peers::Peers::new(id, observation.players)?;
    let mut peer_updates = observation.updates;
    let initial_peers = peer_views.initial(&interest);
    tracing::debug!(
        player = id,
        chunks = interest.len(),
        changes = initial_peers.len(),
        "initial peer projection"
    );
    super::peers::send(conn, initial_peers).await?;
    let mut view_player = lease.player.clone();
    let mut view_center = (0, 0);
    let mut packet_sequence = 0u64;
    loop {
        tokio::select! {
            event=peer_updates.recv()=>{
                let event=event.context("Bedrock peer event backlog; reconnect")?;
                let changes=peer_views.event(event.as_ref(),&interest)?;
                super::peers::send(conn,changes).await?;
            }
            packet = packets.next(conn) => {
                let packet = packet?;
                if world.observes_packets() {
                    packet_sequence=packet_sequence.checked_add(1).context("Bedrock packet observation sequence exhausted")?;
                    world.observe_packet(id,super::catalog::receipt(&packet,packet_sequence)).await?;
                }
                if session_control(conn, &packet, id).await? {
                    continue;
                }
                if let V2225::TextPacket(text)=&packet {
                    if let bedrock::protocol::v924::enums::TextPacketType::Chat {message,..}=&text.message_type
                        && let Err(error)=world.chat(id,message.clone()).await {
                        tracing::debug!(player=id,%error,"Bedrock chat rejected");
                    }
                    continue;
                }
                if let V2225::CommandRequestPacket(request)=&packet {
                    let result=match super::commands::input(request,id) {
                        Ok(input)=>world.submit_command(id,input).await,
                        Err(error)=>Err(error),
                    };
                    if let Err(error)=result {tracing::debug!(player=id,%error,"Bedrock command rejected");}
                    continue;
                }
                if let V2225::ModalFormResponsePacket(response) = &packet {
                    let choice = match (&response.json_response, &response.form_cancel_reason) {
                        (Some(json), None) if json.len() <= 64 => serde_json::from_str::<Option<u32>>(json).ok(),
                        (None, _) => Some(None),
                        _ => None,
                    };
                    if let Some(choice) = choice
                        && let Err(error) = world.submit_form(id, response.form_id, choice).await {
                        tracing::debug!(player=id, %error, "Bedrock form reply rejected");
                    }
                    continue;
                }
                if let V2225::RequestChunkRadiusPacket(request) = &packet {
                    let requested = bounded_radius(request.chunk_radius)?;
                    acknowledge_radius(conn, requested).await?;
                    if requested != radius {
                        spawn::refresh(conn, world, &mut interest, &view_player, requested).await?;
                        radius = requested;
                        super::peers::send(conn,peer_views.refresh(&interest)).await?;
                    }
                    continue;
                }
                let requests=super::inventory::requests(&packet,view_player.selected_slot)?;
                let intents = match super::input::decode(&packet,id) {
                    Ok(intents)=>intents,
                    Err(error) if matches!(&packet,V2225::InventoryTransactionPacket(transaction) if transaction.legacy_request_id!=0 || transaction.legacy_set_item_slots.is_some() || !transaction.actions.is_empty() || !matches!(transaction.data,bedrock::protocol::v662::types::InventoryTransactionData::ItemUse(_)))=>{
                        tracing::debug!(player=id,%error,"unsupported legacy inventory request resynchronized");
                        conn.send(&output::inventory(&view_player)?).await?;
                        conn.send(&output::inventory_ui(&view_player)?).await?;
                        continue;
                    }
                    Err(error)=>{let _=disconnect(conn,"Sculith: invalid or unsupported gameplay request.").await;return Err(error);}
                };
                if let Some(requests)=requests {
                    for request in requests {
                        let outcome=world.inventory_request(id,request).await?;
                        conn.send(&[super::inventory::response(&outcome.response)?]).await?;
                        view_player=outcome.player;
                        conn.send(&output::inventory(&view_player)?).await?;
                        conn.send(&output::inventory_ui(&view_player)?).await?;
                    }
                }
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
                                    if center!=view_center {spawn::refresh(conn,world,&mut interest,&player,radius).await?;view_center=center;super::peers::send(conn,peer_views.refresh(&interest)).await?;}
                                    view_player=player;
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
                            view_player=player;
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
                    WorldEvent::InventoryUpdated {player_id}=>{
                        if *player_id==id {
                            view_player=world.player_snapshot(id).await?;
                            conn.send(&output::inventory(&view_player)?).await?;
                            conn.send(&output::inventory_ui(&view_player)?).await?;
                        }
                    }
                    WorldEvent::PluginInventoryResult {..}=>{},
                    WorldEvent::Chat(message)=>conn.send(&[output::chat(message)]).await?,
                    WorldEvent::Blocks {batch,revisions,..}=>{
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
                    WorldEvent::Message {player_id,message}=>{
                        if *player_id == id { conn.send(&[output::system_message(message)?]).await?; }
                    }
                    WorldEvent::FormRequested {player_id,form_id,form}=>{
                        if *player_id == id { conn.send(&[output::simple_form(*form_id, form)?]).await?; }
                    }
                    WorldEvent::Sound(effect)=>{
                        if effect.player_id == id {
                            let native = settings.content.as_ref().and_then(|content| content.bundle.report.converted_sounds.iter().find(|entry| entry.java == effect.sound))
                                .context("registered sound missing from native bundle")?;
                            conn.send(&[output::sound(effect, &native.bedrock)]).await?;
                        }
                    }
                    WorldEvent::ModelShown(view)=>{
                        if view.player_id==id {
                            let entry=settings.content.as_ref().and_then(|c| c.bundle.report.converted_models.iter().find(|entry| entry.java==view.model)).context("registered model missing from native bundle")?;
                            conn.send(&output::show_model(view,&entry.bedrock)).await?;
                        }
                    }
                    WorldEvent::ModelHidden {player_id,entity_id}=>{
                        if *player_id==id { conn.send(&[output::remove_model(*entity_id)]).await?; }
                    }
                    WorldEvent::FormSubmitted(_) | WorldEvent::Equipped(_) | WorldEvent::PacketReceived {..} | WorldEvent::CommandSubmitted(_) | WorldEvent::InventoryChanged {..}=>{},
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
    conn: &mut Observed<'_, T>,
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
    conn: &mut Observed<'_, T>,
    packets: &mut Packets,
    authentication: &Authentication,
    world: &WorldHandle,
    stage: &mut &'static str,
    protection: &Protection,
    content: Option<&crate::content::Content>,
) -> Result<(PlayerLease, spawn::Interest, i32)> {
    let login = login(conn, packets, authentication, stage, protection).await?;
    tracing::info!(player = %login.authentication.identity().display_name,
        authenticated = login.authentication.is_authenticated(), protection = protection.label(), "Bedrock login completed");
    *stage = "resource-packs";
    packets.set_phase(sculith_core::connections::PacketPhase::ResourcePacks);
    packs::negotiate(conn, packets, content).await?;
    *stage = "world-spawn";
    packets.set_phase(sculith_core::connections::PacketPhase::Spawn);
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
    let mut initial = spawn::initial_packets(&lease.player)?;
    initial.push(super::commands::available(&world.command_specs().await?));
    if let Some(content) = content
        && !content.bundle.report.converted_models.is_empty()
    {
        initial.push(output::model_registry(
            &content.bundle.report.converted_models,
        ));
    }
    conn.send(&initial).await?;
    *stage = "chunk-request";
    let mut interest = None;
    let mut side_packets = 0usize;
    let mut ignored_notifications = 0usize;
    loop {
        let packet = packets.next(conn).await?;
        side_packets += 1;
        ensure!(
            side_packets <= 4096,
            "too many packets before world initialization"
        );
        if session_control(conn, &packet, entity_id).await? {
            continue;
        }
        match packet {
            V2225::RequestChunkRadiusPacket(request) => {
                let radius = bounded_radius(request.chunk_radius)?;
                if let Some((ref mut chunks, ref mut current)) = interest {
                    // Real clients may negotiate again while the first chunk
                    // batch is loading. Do not resend spawn or duplicate chunks.
                    acknowledge_radius(conn, radius).await?;
                    if radius != *current {
                        spawn::refresh(conn, world, chunks, &lease.player, radius).await?;
                        *current = radius;
                    }
                } else {
                    interest = Some((spawn::send_chunks(conn, world, radius).await?, radius));
                }
                *stage = "spawn-acknowledgement";
            }
            V2225::SetLocalPlayerAsInitializedPacket(initialized) if interest.is_some() => {
                if initialized.player_id.0 != entity_id {
                    tracing::warn!(
                        expected_entity_id = entity_id,
                        received_entity_id = initialized.player_id.0,
                        "Bedrock initialization entity ID mismatch"
                    );
                    bail!("client initialized another entity ID");
                }
                tracing::info!(player = %login.authentication.identity().display_name, entity_id,
                    "Bedrock client acknowledged the flat-world spawn");
                world.activate(entity_id).await?;
                packets.trace.attach(entity_id);
                packets.set_phase(sculith_core::connections::PacketPhase::Play);
                let (interest, radius) = interest.expect("checked above");
                return Ok((lease, interest, radius));
            }
            // Unordered movement may race the reliable initialization ack.
            // Pending players cannot mutate the owner; fresh movement is
            // accepted only after activation in the gameplay loop.
            // Hover/inventory notifications are not authority to change a
            // block, entity or inventory; ignore them while chunks load.
            V2225::PlayerAuthInputPacket(_) | V2225::InteractPacket(_) => {}
            packet @ (V2225::MobEquipmentPacket(_)
            | V2225::PlayerHotbarPacket(_)
            | V2225::PlayerActionPacket(_)
            | V2225::AnimatePacket(_)) => {
                // Validate actor IDs and numeric inputs, but never apply any
                // gameplay intent until the reliable initialization ack.
                super::input::decode(&packet, entity_id)?;
            }
            packet @ (V2225::LoginPacket(_)
            | V2225::RequestNetworkSettingsPacket(_)
            | V2225::ClientToServerHandshakePacket(_)
            | V2225::ServerToClientHandshakePacket(_)
            | V2225::SetLocalPlayerAsInitializedPacket(_)) => {
                tracing::warn!(
                    packet_id = packet.id(),
                    stage = *stage,
                    "Bedrock unexpected packet before world initialization"
                );
                bail!(
                    "unexpected packet {} before world initialization",
                    packet.id()
                );
            }
            packet => {
                // Auth and pack negotiation already finished. The official
                // client may send ancillary status while rendering chunks.
                // Discard bounded notifications: never queue/apply game intent
                // or advance initialization on any of these packets.
                ignored_notifications += 1;
                if ignored_notifications <= 8 {
                    tracing::debug!(
                        packet_id = packet.id(),
                        stage = *stage,
                        "Bedrock notification ignored while awaiting initialization"
                    );
                }
            }
        }
    }
}

fn bounded_radius(requested: i32) -> Result<i32> {
    ensure!(requested > 0, "invalid requested chunk radius");
    Ok(requested.min(spawn::RADIUS))
}

async fn acknowledge_radius<T: Transport>(conn: &mut Observed<'_, T>, radius: i32) -> Result<()> {
    conn.send(&[V2225::ChunkRadiusUpdatedPacket(Box::new(
        bedrock::protocol::v662::packets::ChunkRadiusUpdatedPacket {
            chunk_radius: radius,
        },
    ))])
    .await?;
    Ok(())
}

/// Non-mutating control messages can interleave with loading and gameplay.
/// No credentials or arbitrary client diagnostic strings are written to logs.
async fn session_control<T: Transport>(
    conn: &mut Observed<'_, T>,
    packet: &V2225,
    entity_id: u64,
) -> Result<bool> {
    match packet {
        V2225::DisconnectPacket(disconnect) => {
            tracing::info!(reason=?disconnect.reason, "Bedrock client disconnected");
            bail!("Bedrock client disconnected");
        }
        V2225::NetworkStackLatencyPacket(latency) => {
            if latency.is_from_server {
                conn.send(&[V2225::NetworkStackLatencyPacket(Box::new(
                    bedrock::protocol::v662::packets::NetworkStackLatencyPacket {
                        creation_time: latency.creation_time,
                        is_from_server: false,
                    },
                ))])
                .await?;
            }
        }
        V2225::EmoteListPacket(emotes) => {
            ensure!(
                emotes.runtime_id.0 == entity_id,
                "emote list targets another entity"
            );
            ensure!(emotes.emote_piece_ids.len() <= 512, "too many emotes");
        }
        V2225::ServerBoundLoadingScreenPacket(_)
        | V2225::ClientCacheStatusPacket(_)
        | V2225::ServerBoundDiagnosticsPacket(_) => {}
        V2225::PacketViolationWarningPacket(warning) => {
            tracing::warn!(violation_type=?warning.violation_type,
                severity=?warning.violation_severity, violating_packet=?warning.violating_packet_id,
                context_bytes=warning.violation_context.len(),
                "Bedrock client reported a server packet violation");
            bail!("client reported a server packet violation");
        }
        _ => return Ok(false),
    }
    Ok(true)
}

async fn login<T: Transport>(
    conn: &mut Observed<'_, T>,
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
    packets.set_phase(sculith_core::connections::PacketPhase::Login);
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
    packets.set_phase(sculith_core::connections::PacketPhase::Encryption);
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
