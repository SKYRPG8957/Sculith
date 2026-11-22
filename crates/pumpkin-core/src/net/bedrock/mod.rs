mod horse_jump;
pub mod nethernet;
pub mod play;
pub(crate) mod recipe;
use crossbeam::atomic::AtomicCell;
use std::{
    collections::{HashMap, VecDeque},
    io::{Cursor, Error, Write},
    net::SocketAddr,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use tracing::{debug, error, warn};

use bytes::Bytes;
use pumpkin_config::networking::compression::CompressionInfo;
use pumpkin_inventory::screen_handler::ScreenHandler;
use pumpkin_protocol::{
    BClientPacket, PacketDecodeError, RawPacket,
    bedrock::{
        SubClient,
        client::{
            client_cache_miss_response::{CClientCacheMissResponse, MissingBlobData},
            disconnect::CDisconnect,
        },
        network_stack_latency::NetworkStackLatency,
        packet_decoder::BedrockBatchDecoder,
        packet_encoder::BedrockBatchEncoder,
        server::{
            actor_event::SActorEvent, actor_pick_request::SActorPickRequest, animate::SAnimate,
            block_actor_data::SBlockActorData, block_pick_request::SBlockPickRequest,
            client_cache_blob_status::SClientCacheBlobStatus,
            client_cache_status::SClientCacheStatus,
            client_movement_prediction_sync::SClientMovementPredictionSync,
            command_request::SCommandRequest, container_close::SContainerClose,
            cursor_item_drag::SCursorItemDrag, emote::SEmote, emote_list::SEmoteList,
            interact::SInteract, inventory_transaction::SInventoryTransaction,
            item_stack_request::SItemStackRequest, loading_screen::SLoadingScreen, login::SLogin,
            matchmaking_cancel::SMatchmakingCancel, mob_equipment::SMobEquipment,
            modal_form_response::SModalFormResponse,
            packet_violation_warning::SPacketViolationWarning, player_action::SPlayerAction,
            player_auth_input::SPlayerAuthInput, player_hotbar::SPlayerHotbar,
            register_audio_content::SRegisterAudioContent, request_ability::SRequestAbility,
            request_chunk_radius::SRequestChunkRadius,
            request_network_settings::SRequestNetworkSettings,
            resource_pack_chunk_request::SResourcePackChunkRequest,
            resource_pack_client_response::SResourcePackClientResponse, respawn::SRespawn,
            set_local_player_as_initialized::SSetLocalPlayerAsInitialized,
            set_player_inventory_options::SSetPlayerInventoryOptions,
            stonecutter_set_recipe::SStonecutterSetRecipe, text::SText,
        },
    },
    packet::Packet,
    serial::{PacketRead, PacketReadSlice, PacketWrite},
};
use tokio::{
    sync::mpsc::{Receiver, Sender, UnboundedReceiver, UnboundedSender},
    sync::{Mutex, RwLock, oneshot},
    task::JoinHandle,
};

use tokio_util::task::TaskTracker;

pub mod blob_cache;
mod chunk_delivery;
mod close_signal;
pub mod distant_terrain;
mod experience_pickup;
pub mod level_chunk;
pub mod login;
mod mining;
mod packet_observers;
mod play_ingress;
pub mod versions;
use self::close_signal::CloseSignal;
use self::level_chunk::CLevelChunk;
use self::nethernet::NetherNetSession;
use crate::{
    entity::player::Player,
    net::{
        DisconnectReason, MAX_PENDING_BYTES, PacketHandlerResult, PacketRateLimiter,
        chunk_sender::{ChunkSendOutcome, DeliveryFailure},
        decrement_pending_bytes,
    },
    plugin::{
        api::events::world::chunk_send::ChunkSend,
        server::packet::{
            BedrockConnectionPacketReceivedEvent, BedrockConnectionPacketSentEvent, PacketSentEvent,
        },
    },
    server::{
        Server,
        player_commands::{PlayerCommandIngress, submit_inbound_player_command},
    },
};
use arc_swap::{ArcSwap, ArcSwapOption};
use pumpkin_protocol::{bedrock::server::login::ClientData, codec::var_uint::VarUInt};
use pumpkin_util::math::vector2::Vector2;
use pumpkin_util::version::BedrockMinecraftVersion;
use pumpkin_world::{CURRENT_BEDROCK_MC_PROTOCOL, level::SyncChunk};
pub use versions::BedrockVersions;

pub struct OutgoingPacket {
    pub data: Bytes,
    pub completion: Option<oneshot::Sender<()>>,
}

impl OutgoingPacket {
    pub const fn normal(data: Bytes) -> Self {
        Self {
            data,
            completion: None,
        }
    }

    pub const fn priority(data: Bytes, completion: oneshot::Sender<()>) -> Self {
        Self {
            data,
            completion: Some(completion),
        }
    }
}

// A crowded tick can contain both a position and a velocity for each visible actor.
// The byte limit in outgoing_frame_len still applies to every frame.
const MAX_BATCH_PACKETS: usize = 256;
const RETAINED_BATCH_CAPACITY: usize = 16 * 1024;

// Terrain and gameplay share an ordered stream. Leave room for gameplay while
// there is already a small window of data waiting in the queue or at SCTP.
const TERRAIN_SEND_WINDOW_BYTES: usize = 8 * 1024;
const TERRAIN_CREDIT_POLL_INTERVAL: Duration = Duration::from_millis(20);

// Network settings, own impulses, teleports and game actions bypass the motion timer.
fn ordinary_pose(packet: &OutgoingPacket, own_runtime_id: Option<u64>) -> bool {
    use pumpkin_data::bedrock_packet::{MOVE_ACTOR_DELTA, MOVE_PLAYER, SET_ACTOR_MOTION};
    use pumpkin_protocol::codec::var_ulong::VarULong;
    if packet.completion.is_some() {
        return false;
    }
    let Some((&id, mut data)) = packet.data.split_first() else {
        return false;
    };
    match i32::from(id) {
        MOVE_PLAYER => {
            VarULong::read(&mut data).is_ok() && matches!(data.get(24..), Some([0, 0..=1, 0, 0, 0]))
        }
        MOVE_ACTOR_DELTA => matches!(data.last_chunk::<5>(), Some([0..=1, 0, 0, 0, 1])),
        SET_ACTOR_MOTION => {
            // Watcher velocities have three floats and tick zero. Own knockback and
            // prediction corrections can use this same packet ID, including at tick zero.
            own_runtime_id.is_some_and(|own| {
                VarULong::read(&mut data).is_ok_and(|target| target.0 != own)
                    && data.len() == 13
                    && data[12] == 0
            })
        }
        _ => false,
    }
}

fn outgoing_frame_len(packets: &[OutgoingPacket]) -> usize {
    let mut size = 0;
    for (index, packet) in packets.iter().enumerate() {
        let len = packet.data.len() + VarUInt::MAX_SIZE.get();
        if packet.completion.is_some() || len > 8192 - size {
            return index.max(1);
        }
        size += len;
    }
    packets.len()
}

/// Active resource pack staged for chunk streaming to this client.
#[derive(Clone)]
pub struct BedrockStreamingPack {
    pub pack_id: String,
    pub chunk_size: u32,
    pub chunk_count: u32,
    pub total_size: u64,
    pub data: Arc<[u8]>,
}

type TranslatedPacket = (Option<Bytes>, Vec<Bytes>);

/// Frames a raw Bedrock packet as `VarUInt(packet_id) + payload` for the outgoing queue.
#[must_use]
pub fn frame_raw_packet(packet_id: i32, payload: &[u8]) -> Option<Bytes> {
    let id = u16::try_from(packet_id).ok()?;
    let id_varint = VarUInt(u32::from(id));
    let mut buf = Vec::with_capacity(id_varint.written_size() + payload.len());
    id_varint.write(&mut buf).ok()?;
    buf.extend_from_slice(payload);
    Some(Bytes::from(buf))
}

/// Splits an internal `VarUInt(packet_id) + payload` buffer back into `(packet_id, payload)`.
#[must_use]
pub fn split_raw_packet(packet_data: &Bytes) -> Option<(u16, Bytes)> {
    let mut reader = &packet_data[..];
    let packet_id = VarUInt::read(&mut reader).ok()?.0;
    let packet_id = u16::try_from(packet_id).ok()?;
    let payload = packet_data.slice(packet_data.len() - reader.len()..);
    Some((packet_id, payload))
}

struct CursorDrag {
    screen: Weak<std::sync::Mutex<dyn ScreenHandler>>,
    world: Weak<crate::world::World>,
}

struct ClientActivity {
    last_seen: AtomicCell<Instant>,
    horse_jump: std::sync::Mutex<horse_jump::HorseJumpInput>,
    packet_observers: std::sync::OnceLock<packet_observers::PacketObservers>,
    inbound_head: std::sync::Mutex<Option<play_ingress::QueuedPlayInput>>,
    regional_work: std::sync::Mutex<()>,
    chunk_view_revision: AtomicU64,
    published_chunk_view: AtomicU64,
    last_chunk_step: AtomicCell<Option<Instant>>,
    experience_pickup: AtomicCell<Option<experience_pickup::PickupClock>>,
    mining: std::sync::Mutex<Option<mining::MiningClock>>,
}

pub struct BedrockClient {
    pub id: u64,
    session: Arc<NetherNetSession>,
    /// The client's IP address.
    pub address: SocketAddr,
    pub player: ArcSwap<Option<Arc<Player>>>,
    pub protocol_version: AtomicU32,
    pub version: AtomicCell<BedrockMinecraftVersion>,
    pub client_data: ArcSwap<Option<Arc<ClientData>>>,
    server: std::sync::Weak<Server>,
    /// All Bedrock clients
    /// This list is used to remove the client if the connection gets closed
    pub be_clients: Arc<Mutex<HashMap<SocketAddr, Arc<Self>>>>,

    tasks: TaskTracker,
    rt_handle: tokio::runtime::Handle,
    outgoing_packet_queue_send: UnboundedSender<OutgoingPacket>,
    /// FIFO queue of serialized packets for the network
    outgoing_packet_queue_recv: Mutex<Option<UnboundedReceiver<OutgoingPacket>>>,

    /// Tracks total buffered payload bytes in the outgoing queues.
    pub pending_bytes: Arc<AtomicUsize>,
    listed_players: std::sync::Mutex<HashMap<uuid::Uuid, Bytes>>,

    /// The packet encoder for outgoing packets.
    network_writer: Arc<RwLock<BedrockBatchEncoder>>,
    /// The packet decoder for incoming packets.
    network_reader: Mutex<BedrockBatchDecoder>,

    /// The next form ID to use for custom forms.
    pub next_form_id: AtomicU32,
    pub inventory_opened: AtomicBool,
    cursor_drag: std::sync::Mutex<Option<CursorDrag>>,
    /// Last processed `PlayerAuthInput` tick. Motion for the own player must carry it.
    pub input_tick: AtomicU64,
    /// Client predicted velocity from the last `PlayerAuthInput`. Pushes build on it.
    pub client_delta: AtomicCell<pumpkin_util::math::vector3::Vector3<f64>>,
    /// World age of the last server motion sent to the own player.
    pub own_motion_age: AtomicCell<i64>,
    /// `PlayerAuthInput` tick tagged on that last own-player `SetActorMotion`.
    pub own_motion_tick: AtomicU64,
    /// Push velocity the client carries from the server's motions.
    pub push_residual: std::sync::Mutex<crate::entity::player::client_push::PushResidual>,
    /// Ids and send times of unanswered `NetworkStackLatency` probes, oldest first.
    pending_latencies: std::sync::Mutex<VecDeque<(u64, Instant)>>,
    next_latency_id: AtomicU64,
    last_latency_send: AtomicCell<Instant>,
    /// Inputs since the last teleport without reaching it. Resent past a threshold.
    pub teleport_unconfirmed_inputs: AtomicU32,
    /// Network position of the last own teleport `MovePlayer`, which the client must reach.
    pub teleport_sent_pos: AtomicCell<pumpkin_util::math::vector3::Vector3<f32>>,
    /// Separate from normal vitals caching so the first rejected use always gets corrected.
    last_food_rejection_tick: AtomicCell<Option<i32>>,
    pub client_cache_supported: AtomicBool,
    pub blob_cache: std::sync::Mutex<blob_cache::BlobCache>,
    shared_pack: ArcSwapOption<crate::resource_pack::SharedPack>,
    pub streaming_packs: std::sync::Mutex<HashMap<String, BedrockStreamingPack>>,
    initial_spawn_started: AtomicBool,
    /// Stops producers and wakes the writer to drain its FIFO.
    closing: CloseSignal,
    activity: Arc<ClientActivity>,
    incoming_game_packet_send: Sender<RawPacket>,
    incoming_game_packet_recv: Mutex<Option<Receiver<RawPacket>>>,
    /// Packet rate limiter for incoming client packets.
    pub packet_limiter: PacketRateLimiter,
}

impl BedrockClient {
    /// Allocates an edition-specific form ID below the shared API's reserved range.
    pub fn allocate_form_id(&self) -> Option<u32> {
        self.next_form_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                (next < crate::entity::player::forms::SHARED_FORM_BIT).then(|| next + 1)
            })
            .ok()
    }

    #[must_use]
    pub fn new(
        id: u64,
        session: Arc<NetherNetSession>,
        address: SocketAddr,
        be_clients: Arc<Mutex<HashMap<SocketAddr, Arc<Self>>>>,
        packet_limiter: PacketRateLimiter,
        server: std::sync::Weak<Server>,
    ) -> Self {
        let (send, recv) = tokio::sync::mpsc::unbounded_channel();
        let (incoming_send, incoming_recv) = tokio::sync::mpsc::channel(4096);
        let rt_handle = tokio::runtime::Handle::current();
        Self {
            id,
            session,
            player: ArcSwap::new(Arc::new(None)),
            address,
            protocol_version: AtomicU32::new(0),
            version: AtomicCell::new(BedrockMinecraftVersion::Unknown),
            client_data: ArcSwap::new(Arc::new(None)),
            server,
            be_clients,
            network_writer: Arc::new(RwLock::new(BedrockBatchEncoder::new())),
            network_reader: Mutex::new(BedrockBatchDecoder::new()),
            tasks: TaskTracker::new(),
            rt_handle,
            outgoing_packet_queue_send: send,
            outgoing_packet_queue_recv: Mutex::new(Some(recv)),
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            listed_players: std::sync::Mutex::default(),
            next_form_id: AtomicU32::new(0),
            inventory_opened: AtomicBool::new(false),
            cursor_drag: std::sync::Mutex::new(None),
            input_tick: AtomicU64::new(0),
            client_delta: AtomicCell::new(pumpkin_util::math::vector3::Vector3::default()),
            own_motion_age: AtomicCell::new(i64::MIN),
            own_motion_tick: AtomicU64::new(0),
            push_residual: std::sync::Mutex::default(),
            pending_latencies: std::sync::Mutex::new(VecDeque::new()),
            next_latency_id: AtomicU64::new(1),
            last_latency_send: AtomicCell::new(Instant::now()),
            teleport_unconfirmed_inputs: AtomicU32::new(0),
            teleport_sent_pos: AtomicCell::new(pumpkin_util::math::vector3::Vector3::default()),
            last_food_rejection_tick: AtomicCell::new(None),
            client_cache_supported: AtomicBool::new(false),
            blob_cache: std::sync::Mutex::new(blob_cache::BlobCache::new()),
            shared_pack: ArcSwapOption::empty(),
            streaming_packs: std::sync::Mutex::new(HashMap::new()),
            initial_spawn_started: AtomicBool::new(false),
            closing: CloseSignal::default(),
            activity: Arc::new(ClientActivity {
                last_seen: AtomicCell::new(Instant::now()),
                horse_jump: std::sync::Mutex::default(),
                packet_observers: std::sync::OnceLock::new(),
                inbound_head: std::sync::Mutex::default(),
                regional_work: std::sync::Mutex::default(),
                chunk_view_revision: AtomicU64::new(0),
                published_chunk_view: AtomicU64::new(0),
                last_chunk_step: AtomicCell::new(None),
                experience_pickup: AtomicCell::new(None),
                mining: std::sync::Mutex::default(),
            }),
            incoming_game_packet_send: incoming_send,
            incoming_game_packet_recv: Mutex::new(Some(incoming_recv)),
            packet_limiter,
        }
    }

    pub async fn get_packet(&self) -> Option<RawPacket> {
        let mut guard = self.incoming_game_packet_recv.lock().await;
        let recv = guard.as_mut()?;
        tokio::select! {
            () = self.await_close_interrupt() => None,
            packet = recv.recv() => packet,
        }
    }

    pub fn start_outgoing_packet_task(self: &Arc<Self>) {
        let client = self.clone();
        self.spawn_task(async move {
            let Some(mut receiver) = client.outgoing_packet_queue_recv.lock().await.take() else { return; };
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
            let mut packets = Vec::with_capacity(64);
            let mut frames = Vec::with_capacity(64);
            let mut encoded = Vec::new();
            loop {
                let count = tokio::select! {
                    count = receiver.recv_many(&mut packets, MAX_BATCH_PACKETS) => count,
                    () = client.closing.wait(), if !receiver.is_closed() => {
                        receiver.close();
                        continue;
                    }
                    _ = interval.tick() => {
                        if !client.tick_connection().await || receiver.is_closed() && receiver.is_empty() { break; }
                        continue;
                    }
                };
                if count == 0 {
                    break;
                }
                let own_id = client.player.load().as_deref()
                    .filter(|_| client.is_current_protocol())
                    .map(|p| p.entity_id() as u64);
                let can_wait = |packet: &OutgoingPacket| ordinary_pose(packet, own_id);
                if count < MAX_BATCH_PACKETS && packets.iter().all(can_wait) {
                    let deadline = tokio::time::sleep(std::time::Duration::from_millis(5));
                    tokio::pin!(deadline);
                    while packets.len() < MAX_BATCH_PACKETS {
                        let start = packets.len();
                        tokio::select! {
                            biased;
                            () = &mut deadline => break,
                            count = receiver.recv_many(&mut packets, MAX_BATCH_PACKETS - start) => {
                                if count == 0 || !packets[start..].iter().all(can_wait) { break; }
                            }
                        }
                    }
                }
                let mut remaining = packets.as_mut_slice();
                while !remaining.is_empty() {
                    let (group, rest) = remaining.split_at_mut(outgoing_frame_len(remaining));
                    let queued = group.iter().map(|p| p.data.len()).sum();
                    let sent = client.send_batch(group, &mut frames, &mut encoded).await;
                    decrement_pending_bytes(&client.pending_bytes, queued);
                    match sent {
                        Ok(true) => for packet in group {
                            if let Some(done) = packet.completion.take() { let _ = done.send(()); }
                        },
                        Ok(false) => {}
                        Err(error) => {
                            decrement_pending_bytes(&client.pending_bytes, rest.iter().map(|p| p.data.len()).sum());
                            warn!("Failed to send Bedrock batch to {}: {error}", client.address);
                            client.closing.close();
                            client.finish_close().await;
                            return;
                        }
                    }
                    remaining = rest;
                }
                packets.clear();
            }
            // Synchronous kicks queue the final disconnect, then request a drain.
            if client.closing.is_closed() && !client.session.is_closed() {
                let _ = client.session.flush_reliable(std::time::Duration::from_secs(1)).await;
            }
            client.closing.close();
            client.finish_close().await;
        });
    }

    async fn send_batch(
        &self,
        group: &[OutgoingPacket],
        frames: &mut Vec<(u16, Bytes)>,
        encoded: &mut Vec<u8>,
    ) -> Result<bool, String> {
        frames.clear();
        let writer = *self.network_writer.read().await;
        if let [packet] = group
            && let Some(encoded) = writer
                .try_share_large_terrain_frame(&packet.data)
                .map_err(|e| e.to_string())?
        {
            self.session.send(&encoded[1..]).await?;
            return Ok(true);
        }
        encoded.clear();
        // Broadcasts already share the complete header and payload across viewers.
        // A single pack chunk can reuse its separately cached compressed frame below.
        let single_chunk = matches!(group, [packet] if packet.data.first().is_some_and(|&id|
            i32::from(id) == pumpkin_data::bedrock_packet::LEVEL_CHUNK));
        if (group.len() < 2 && !single_chunk)
            || !writer
                .try_write_framed_game_packets(
                    group.iter().map(|packet| packet.data.as_ref()),
                    &mut *encoded,
                )
                .map_err(|e| e.to_string())?
        {
            for packet in group {
                if let Some(frame) = split_raw_packet(&packet.data) {
                    frames.push(frame);
                } else {
                    warn!(
                        "Dropping malformed outgoing Bedrock packet for {}",
                        self.address
                    );
                }
            }
            if frames.is_empty() {
                return Ok(false);
            }
            if let [(id, body)] = frames.as_slice()
                && i32::from(*id) == pumpkin_data::bedrock_packet::RESOURCE_PACK_CHUNK_DATA
                && body.len() <= crate::resource_pack::BEDROCK_PACK_CHUNK_SIZE as usize + 512
                && let Some(pack) = self.shared_pack.load_full()
            {
                let encoded = pack
                    .chunk_frame(writer, body)
                    .await
                    .map_err(|e| e.to_string())?;
                self.session.send(encoded.as_ref()).await?;
                return Ok(true);
            }
            writer
                .write_game_packets(frames, SubClient::Main, SubClient::Main, &mut *encoded)
                .map_err(|e| e.to_string())?;
        }
        self.session.send(&encoded[1..]).await?;
        if encoded.capacity() > RETAINED_BATCH_CAPACITY {
            *encoded = Vec::new();
        }
        Ok(true)
    }

    async fn tick_connection(&self) -> bool {
        use std::time::Duration;
        let idle = self.activity.last_seen.load().elapsed();
        if idle > Duration::from_secs(10) {
            // A downloading client may stay silent until its full reliable chunk arrives.
            if idle < Duration::from_secs(60)
                && !self
                    .streaming_packs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_empty()
            {
                return true;
            }
            debug!("Bedrock client {} timed out", self.address);
            self.close().await;
            return false;
        }
        true
    }

    pub async fn process_nethernet_packet(self: &Arc<Self>, server: &Arc<Server>, packet: Bytes) {
        self.activity.last_seen.store(Instant::now());
        if let Err(error) = self.process_batch(server, packet).await {
            error!(
                "Failed to handle NetherNet payload for {}: {error}",
                self.address
            );
            self.kick(DisconnectReason::BadPacket, error.to_string())
                .await;
        }
    }

    pub fn nethernet_public_key(&self) -> Option<&pumpkin_auth::p384::PublicKey> {
        self.session.client_public_key()
    }

    pub async fn set_compression(&self, compression: CompressionInfo) {
        self.set_inbound_compression(compression.threshold).await;
        self.set_outbound_compression(compression).await;
    }

    async fn set_inbound_compression(&self, threshold: u32) {
        self.network_reader
            .lock()
            .await
            .set_compression(threshold as usize);
    }

    async fn set_outbound_compression(&self, compression: CompressionInfo) {
        self.network_writer
            .write()
            .await
            .set_compression((compression.threshold as usize, compression.level));
    }

    pub async fn set_prefixed_compression(&self, prefixed: bool) {
        self.network_reader
            .lock()
            .await
            .set_prefixed_compression(prefixed);

        self.network_writer
            .write()
            .await
            .set_prefixed_compression(prefixed);
    }

    pub fn try_kick(&self, reason: DisconnectReason, message: String) {
        warn!("Closing connection for {}: {message}", self.address);
        let packet = CDisconnect::new(reason as i32, message);
        if let Some(data) = self
            .serialize_packet(&packet)
            .ok()
            .and_then(|data| self.translate_outgoing(data, false).0)
        {
            let packet_len = data.len();
            let _ = self.pending_bytes.fetch_add(packet_len, Ordering::AcqRel);
            self.queue_outgoing(OutgoingPacket::normal(data), packet_len);
        }
        self.closing.close();
    }

    pub async fn kick(&self, reason: DisconnectReason, message: String) {
        warn!("Closing connection for {}: {message}", self.address);
        self.send_packet(&CDisconnect::new(reason as i32, message))
            .await;
        let _ = self
            .session
            .flush_reliable(std::time::Duration::from_secs(1))
            .await;
        self.close().await;
    }

    pub async fn kick_explicit(
        &self,
        reason: DisconnectReason,
        message: String,
        skip_message: bool,
        filtered_message: String,
        send_packet: bool,
    ) {
        warn!("Closing connection for {}: {message}", self.address);
        if send_packet {
            self.send_packet(&CDisconnect {
                reason: pumpkin_protocol::codec::var_int::VarInt(reason as i32),
                skip_message,
                message,
                filtered_message,
            })
            .await;
            let _ = self
                .session
                .flush_reliable(std::time::Duration::from_secs(1))
                .await;
        }
        self.close().await;
    }

    /// Queues the chunks and reports which were sent and why the others were not.
    /// With an `epoch`, chunks are dropped if the player's chunk epoch moved on (world change)
    /// while they were being encoded.
    pub async fn send_chunks(&self, chunks: &[SyncChunk], epoch: Option<u32>) -> ChunkSendOutcome {
        let mut outcome = ChunkSendOutcome::default();
        let player = self.player.load_full();
        let Some(player) = player.as_ref() else {
            debug!(
                "send_chunks: player not set yet, deferring {} chunks",
                chunks.len()
            );
            return outcome;
        };
        let Some(server) = player.world().server.upgrade() else {
            return outcome;
        };
        if let Some(epoch) = epoch
            && !self.wait_for_terrain_credit(player, epoch).await
        {
            return outcome;
        }

        let world = player.world();
        let valid_chunks = if server.plugin_manager.has_handlers::<ChunkSend>() {
            let valid = ChunkSend::filter(&server, &world, chunks).await;
            outcome.record_cancelled(chunks, &valid);
            std::borrow::Cow::Owned(valid)
        } else {
            std::borrow::Cow::Borrowed(chunks)
        };
        if valid_chunks.is_empty() {
            return outcome;
        }
        let bedrock_dimension = if world.dimension == pumpkin_data::dimension::Dimension::THE_NETHER
        {
            1
        } else if world.dimension == pumpkin_data::dimension::Dimension::THE_END {
            2
        } else {
            0
        };
        let cache_enabled = server.advanced_config.networking.bedrock.chunk_caching
            && self.client_cache_supported.load(Ordering::Relaxed);

        let (encoded_payloads, new_blobs) = Self::prepare_chunk_payloads(
            &server,
            world,
            &valid_chunks,
            bedrock_dimension,
            cache_enabled,
        )
        .await;
        let translated_chunks = self.translate_chunk_payloads(encoded_payloads).await;

        // Checked and queued under the sender lock: a world change resets the sender under it
        // before queueing `CChangeDimension`, so no old-world chunk can land behind that packet
        let sender = epoch.map(|_| {
            player
                .chunk_sender
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        if epoch.is_some_and(|epoch| player.chunk_send_epoch.load(Ordering::Relaxed) != epoch) {
            outcome.failed.extend(
                translated_chunks
                    .into_iter()
                    .map(|(pos, _)| (pos, DeliveryFailure::Deferred)),
            );
            return outcome;
        }

        if !new_blobs.is_empty() {
            let mut cache = self
                .blob_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (hash, blob) in new_blobs {
                cache.insert(hash, blob);
            }
        }

        let still_watched = translated_chunks.into_iter().filter(|(pos, _)| {
            sender
                .as_ref()
                .is_none_or(|sender| sender.is_chunk_sent(pos))
        });
        self.queue_translated_chunks(still_watched, &mut outcome);
        outcome
    }

    async fn wait_for_terrain_credit(&self, player: &Player, epoch: u32) -> bool {
        while !self.closing.is_closed() && player.chunk_send_epoch.load(Ordering::Relaxed) == epoch
        {
            let queued = self.pending_bytes.load(Ordering::Acquire);
            if queued < TERRAIN_SEND_WINDOW_BYTES {
                let Some(outstanding) = self.session.reliable_outstanding_bytes().await else {
                    return false;
                };
                if outstanding < TERRAIN_SEND_WINDOW_BYTES - queued {
                    return true;
                }
            }
            tokio::select! {
                () = self.closing.wait() => return false,
                () = tokio::time::sleep(TERRAIN_CREDIT_POLL_INTERVAL) => {}
            }
        }
        false
    }

    async fn prepare_chunk_payloads(
        server: &Server,
        world: Arc<crate::world::World>,
        valid_chunks: &[SyncChunk],
        bedrock_dimension: i32,
        cache_enabled: bool,
    ) -> (
        Vec<(Vector2<i32>, Option<Bytes>)>,
        Vec<level_chunk::ChunkBlob>,
    ) {
        let mut encoded_payloads = Vec::with_capacity(valid_chunks.len());
        let mut misses = Vec::new();
        for chunk in valid_chunks {
            let framed = if cache_enabled {
                None
            } else {
                world
                    .bedrock_chunk_frames
                    .get(chunk, bedrock_dimension)
                    .filter(|_| !world.chunk_has_block_entities(chunk))
            };
            if framed.is_none() {
                misses.push((encoded_payloads.len(), chunk.clone()));
            }
            encoded_payloads.push((Vector2::new(chunk.x, chunk.z), framed));
        }

        let mut new_blobs = Vec::new();
        if !misses.is_empty() {
            let (tx, rx) = oneshot::channel();
            server.compute_pool.spawn(move || {
                let mut results = Vec::with_capacity(misses.len());
                let mut blobs = Vec::new();
                for (index, chunk) in misses {
                    let revision = chunk.section.terrain_revision();
                    let actors = world.bedrock_chunk_block_actors(&chunk);
                    // Other viewers may have encoded this chunk while this job was queued.
                    if !cache_enabled && actors.is_empty()
                        && let Some(frame) = world.bedrock_chunk_frames.get(&chunk, bedrock_dimension)
                    {
                        results.push((index, frame));
                        continue;
                    }
                    match CLevelChunk::encode_chunk(&chunk, bedrock_dimension, cache_enabled, &actors) {
                        Ok((payload, chunk_blobs)) => {
                            if let Some(frame) = frame_raw_packet(CLevelChunk::PACKET_ID, &payload) {
                                if !cache_enabled && actors.is_empty() {
                                    world.bedrock_chunk_frames.insert(&chunk, bedrock_dimension, revision, &frame);
                                }
                                results.push((index, frame));
                                blobs.extend(chunk_blobs);
                            }
                        }
                        Err(error) => error!(%error, x=chunk.x, z=chunk.z, "Failed to serialize Bedrock chunk"),
                    }
                }
                let _ = tx.send((results, blobs));
            });
            if let Ok((results, blobs)) = rx.await {
                for (index, frame) in results {
                    encoded_payloads[index].1 = Some(frame);
                }
                new_blobs = blobs;
            }
        }
        (encoded_payloads, new_blobs)
    }

    async fn translate_chunk_payloads(
        &self,
        encoded_payloads: Vec<(Vector2<i32>, Option<Bytes>)>,
    ) -> Vec<(Vector2<i32>, Option<TranslatedPacket>)> {
        let mut translated_chunks = Vec::with_capacity(encoded_payloads.len());
        for (pos, payload) in encoded_payloads {
            let Some(framed) = payload else {
                translated_chunks.push((pos, None));
                continue;
            };
            let translated = self.translate_outgoing_async(framed, true).await;
            translated_chunks.push((pos, Some(translated)));
        }
        translated_chunks
    }

    fn queue_translated_chunks(
        &self,
        translated_chunks: impl IntoIterator<Item = (Vector2<i32>, Option<TranslatedPacket>)>,
        outcome: &mut ChunkSendOutcome,
    ) {
        for (pos, translated) in translated_chunks {
            let queued = match translated {
                Some((Some(packet), extras)) => Some(self.queue_translated_packet(packet, extras)),
                // Cancelled by a PacketSentEvent handler: queue any replacement packets and mark
                // sent so the chunk sender does not endlessly retry an intentionally replaced chunk.
                Some((None, extras)) => Some(self.queue_extra_packets(extras)),
                None => None,
            };
            match queued {
                Some(true) => outcome.sent.push(pos),
                Some(false) => outcome.failed.push((pos, DeliveryFailure::Deferred)),
                None => outcome.failed.push((pos, DeliveryFailure::Unencodable)),
            }
        }
    }

    pub fn set_player(&self, player: Arc<Player>) {
        self.player.store(Arc::new(Some(player)));
    }

    pub fn clear_player(&self) {
        self.player.store(Arc::new(None));
    }

    pub(crate) fn is_current_protocol(&self) -> bool {
        let protocol = self.protocol_version.load(Ordering::Relaxed);
        protocol == 0 || protocol == CURRENT_BEDROCK_MC_PROTOCOL
    }

    fn prepare_play_sent_event(
        &self,
        packet_data: &Bytes,
        observe: bool,
    ) -> Option<(Arc<Server>, PacketSentEvent)> {
        let current_version = self.is_current_protocol();
        if current_version && !observe {
            return None;
        }
        let player_guard = self.player.load();
        let player = (**player_guard).as_ref()?;
        let server = player
            .world()
            .server
            .upgrade()
            .or_else(|| self.server.upgrade())?;
        if !server.plugin_manager.has_handlers::<PacketSentEvent>() {
            return None;
        }
        let (packet_id, payload) = split_raw_packet(packet_data)?;
        let event = PacketSentEvent::new_raw(player.clone(), i32::from(packet_id), payload);
        Some((server, event))
    }

    fn finish_play_sent_event(packet_data: Bytes, event: &PacketSentEvent) -> TranslatedPacket {
        let extras = event
            .extra_packets
            .iter()
            .filter_map(|(id, payload)| frame_raw_packet(*id, payload))
            .collect();
        if event.cancelled {
            return (None, extras);
        }
        if split_raw_packet(&packet_data).is_some_and(|(id, payload)| {
            event.packet_id == i32::from(id) && event.payload == payload
        }) {
            return (Some(packet_data), extras);
        }
        (frame_raw_packet(event.packet_id, &event.payload), extras)
    }

    fn prepare_connection_sent_event(
        &self,
        packet_data: &Bytes,
    ) -> Option<(Arc<Server>, BedrockConnectionPacketSentEvent)> {
        if self.player.load().is_some() {
            return None;
        }
        let server = self.server.upgrade()?;
        if !server
            .plugin_manager
            .has_handlers::<BedrockConnectionPacketSentEvent>()
        {
            return None;
        }
        let (packet_id, payload) = split_raw_packet(packet_data)?;
        let event = BedrockConnectionPacketSentEvent::new(
            self.id,
            self.protocol_version.load(Ordering::Relaxed),
            self.version.load(),
            i32::from(packet_id),
            payload,
        );
        Some((server, event))
    }

    fn finish_connection_sent_event(event: &BedrockConnectionPacketSentEvent) -> TranslatedPacket {
        let extras = event
            .extra_packets
            .iter()
            .filter_map(|(id, payload)| frame_raw_packet(*id, payload))
            .collect();
        if event.cancelled {
            return (None, extras);
        }
        (frame_raw_packet(event.packet_id, &event.payload), extras)
    }

    fn translate_outgoing(&self, packet_data: Bytes, observe: bool) -> TranslatedPacket {
        if let Some((server, mut event)) = self.prepare_play_sent_event(&packet_data, observe) {
            server.plugin_manager.fire_blocking(&server, &mut event);
            return Self::finish_play_sent_event(packet_data, &event);
        }
        if let Some((server, mut event)) = self.prepare_connection_sent_event(&packet_data) {
            server.plugin_manager.fire_blocking(&server, &mut event);
            return Self::finish_connection_sent_event(&event);
        }
        (Some(packet_data), Vec::new())
    }

    async fn translate_outgoing_async(
        &self,
        packet_data: Bytes,
        observe: bool,
    ) -> TranslatedPacket {
        if let Some((server, mut event)) = self.prepare_play_sent_event(&packet_data, observe) {
            server.plugin_manager.fire(&server, &mut event).await;
            return Self::finish_play_sent_event(packet_data, &event);
        }
        if let Some((server, mut event)) = self.prepare_connection_sent_event(&packet_data) {
            server.plugin_manager.fire(&server, &mut event).await;
            return Self::finish_connection_sent_event(&event);
        }
        (Some(packet_data), Vec::new())
    }

    fn reserve_bytes(&self, packet_len: usize) -> Option<usize> {
        if self.is_closed() {
            return None;
        }
        let prev_bytes = self.pending_bytes.fetch_add(packet_len, Ordering::AcqRel);
        let new_bytes = prev_bytes.saturating_add(packet_len);

        if new_bytes > MAX_PENDING_BYTES {
            decrement_pending_bytes(&self.pending_bytes, packet_len);
            if !self.is_closed() {
                warn!(
                    "Bedrock client {} outbound packet buffer overflow ({} bytes > {} bytes). Closing connection.",
                    self.address, new_bytes, MAX_PENDING_BYTES
                );
                self.closing.close();
            }
            return None;
        }
        Some(packet_len)
    }

    fn queue_outgoing(&self, packet: OutgoingPacket, packet_len: usize) -> bool {
        if let Err(err) = self.outgoing_packet_queue_send.send(packet) {
            decrement_pending_bytes(&self.pending_bytes, packet_len);
            if !self.is_closed() {
                error!("Failed to add packet to the outgoing packet queue for client: {err}");
            }
            return false;
        }
        true
    }

    fn queue_extra_packets(&self, extras: Vec<Bytes>) -> bool {
        for extra in extras {
            let Some(packet_len) = self.reserve_bytes(extra.len()) else {
                return false;
            };
            if !self.queue_outgoing(OutgoingPacket::normal(extra), packet_len) {
                return false;
            }
        }
        !self.is_closed()
    }

    fn queue_translated_packet(&self, packet_data: Bytes, extras: Vec<Bytes>) -> bool {
        let Some(packet_len) = self.reserve_bytes(packet_data.len()) else {
            return false;
        };
        if !self.queue_outgoing(OutgoingPacket::normal(packet_data), packet_len) {
            return false;
        }
        if !extras.is_empty() {
            self.queue_extra_packets(extras);
        }
        true
    }

    pub(crate) fn observes_play_packets(&self) -> bool {
        self.server
            .upgrade()
            .is_none_or(|server| server.plugin_manager.has_handlers::<PacketSentEvent>())
    }

    /// The skin entry must reach the same ordered channel before `AddPlayer`. A
    /// changed skin or a reconnected player produces a fresh shared Bytes allocation.
    pub(crate) fn ensure_listed(&self, subject: &Player) -> bool {
        let Ok(bytes) = subject.bedrock_roster_bytes() else {
            return false;
        };
        let id = subject.gameprofile.id;
        if self.is_closed() {
            return false;
        }
        if !self.is_current_protocol()
            || self.player.load().is_none()
            || self.observes_play_packets()
        {
            self.forget_listed(id);
            return self.try_enqueue_packet_data(bytes);
        }
        let mut known = self
            .listed_players
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if known
            .get(&id)
            .is_some_and(|prior| prior.as_ptr() == bytes.as_ptr())
        {
            return true;
        }
        if self.queue_translated_packet(bytes.clone(), Vec::new()) {
            known.insert(id, bytes);
            return true;
        }
        false
    }

    pub(crate) fn forget_listed(&self, id: uuid::Uuid) {
        self.listed_players
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }

    pub async fn enqueue_packet(&self, packet_data: Bytes) {
        self.enqueue_packet_data(packet_data).await;
    }

    pub fn try_enqueue_packet(&self, packet_data: Bytes) {
        self.try_enqueue_packet_data(packet_data);
    }

    /// Queues a clientbound packet to be sent to the connected client.
    pub async fn enqueue_packet_data(&self, packet_data: Bytes) -> bool {
        if self.is_closed() {
            return false;
        }
        let (packet_data, extras) = self.translate_outgoing_async(packet_data, true).await;
        match packet_data {
            Some(packet_data) => self.queue_translated_packet(packet_data, extras),
            None => self.queue_extra_packets(extras),
        }
    }

    /// `false` when the packet was dropped instead of queued.
    pub fn try_enqueue_packet_data(&self, packet_data: Bytes) -> bool {
        self.try_enqueue_packet_data_inner(packet_data, true)
    }

    /// Like [`Self::try_enqueue_packet_data`] without firing `PacketSentEvent` on current-version
    /// clients, while still running version translation for legacy clients.
    pub fn try_enqueue_packet_data_unobserved(&self, packet_data: Bytes) -> bool {
        self.try_enqueue_packet_data_inner(packet_data, false)
    }

    /// Queues an already-formatted raw Bedrock packet (`packet_id` + client-version `payload`)
    /// directly onto the wire queue without firing `PacketSentEvent`.
    pub fn try_enqueue_raw_packet_direct(&self, packet_id: u16, payload: &[u8]) -> bool {
        let Some(packet_data) = frame_raw_packet(i32::from(packet_id), payload) else {
            return false;
        };
        let Some(packet_len) = self.reserve_bytes(packet_data.len()) else {
            return false;
        };
        self.queue_outgoing(OutgoingPacket::normal(packet_data), packet_len)
    }

    fn try_enqueue_packet_data_inner(&self, packet_data: Bytes, observe: bool) -> bool {
        if !observe && self.is_current_protocol() && self.player.load().is_some() {
            return self.queue_translated_packet(packet_data, Vec::new());
        }
        if self.is_closed() {
            return false;
        }
        let (packet_data, extras) = self.translate_outgoing(packet_data, observe);
        match packet_data {
            Some(packet_data) => self.queue_translated_packet(packet_data, extras),
            None => self.queue_extra_packets(extras),
        }
    }

    pub fn write_raw_packet<P: BClientPacket>(
        packet: &P,
        mut writer: impl Write,
    ) -> Result<(), Error> {
        VarUInt(P::PACKET_ID as u32).write(&mut writer)?;
        packet.write_packet(writer)
    }

    pub async fn write_game_packet<P: BClientPacket>(
        &self,
        packet: &P,
        write: impl Write,
    ) -> Result<(), Error> {
        let mut packet_payload = Vec::new();
        packet.write_packet(&mut packet_payload)?;

        let encoder = self.network_writer.read().await;
        encoder.write_game_packet(
            P::PACKET_ID as u16,
            SubClient::Main,
            SubClient::Main,
            &packet_payload,
            write,
        )
    }

    pub fn serialize_packet<P: BClientPacket>(&self, packet: &P) -> Result<Bytes, Error> {
        let mut buf = Vec::new();
        Self::write_raw_packet(packet, &mut buf)?;
        Ok(buf.into())
    }

    pub async fn send_packet<P: BClientPacket>(&self, packet: &P) {
        match self.serialize_packet(packet) {
            Ok(data) => self.send_game_packet(data).await,
            Err(err) => error!("Failed to serialize Bedrock packet: {err}"),
        }
    }

    pub async fn enqueue_client_packet<P: BClientPacket>(&self, packet: &P) {
        match self.serialize_packet(packet) {
            Ok(data) => self.enqueue_packet(data).await,
            Err(err) => error!("Failed to serialize Bedrock packet: {err}"),
        }
    }

    pub fn try_enqueue_client_packet<P: BClientPacket>(&self, packet: &P) {
        match self.serialize_packet(packet) {
            Ok(data) => self.try_enqueue_packet(data),
            Err(err) => error!("Failed to serialize Bedrock packet: {err}"),
        }
    }

    async fn send_and_wait_translated(&self, (packet_data, extras): (Option<Bytes>, Vec<Bytes>)) {
        if self.is_closed() {
            return;
        }
        let mut packets = Vec::with_capacity(usize::from(packet_data.is_some()) + extras.len());
        if let Some(packet_data) = packet_data {
            packets.push(packet_data);
        }
        packets.extend(extras);
        let Some(last_packet) = packets.pop() else {
            return;
        };
        for packet in packets {
            let Some(packet_len) = self.reserve_bytes(packet.len()) else {
                return;
            };
            if !self.queue_outgoing(OutgoingPacket::normal(packet), packet_len) {
                return;
            }
        }
        let Some(last_len) = self.reserve_bytes(last_packet.len()) else {
            return;
        };
        let (tx, rx) = oneshot::channel();
        if self.queue_outgoing(OutgoingPacket::priority(last_packet, tx), last_len) {
            let _ = rx.await;
        }
    }

    pub async fn send_game_packet(&self, packet_data: Bytes) {
        if self.is_closed() {
            return;
        }
        let translated = self.translate_outgoing_async(packet_data, true).await;
        self.send_and_wait_translated(translated).await;
    }

    pub async fn close(&self) {
        if self.closing.close() {
            self.finish_close().await;
        }
    }

    async fn finish_close(&self) {
        self.session.close().await;
        let mut clients = self.be_clients.lock().await;
        if clients
            .get(&self.address)
            .is_some_and(|c| std::ptr::eq(c.as_ref(), self))
        {
            clients.remove(&self.address);
        }
    }

    pub async fn await_tasks(&self) {
        self.tasks.close();
        self.tasks.wait().await;
    }

    pub fn is_closed(&self) -> bool {
        self.closing.is_closed() || self.session.is_closed()
    }

    pub fn enqueue_spawn_packet(&self, entity: &dyn crate::entity::EntityBase) {
        entity.send_bedrock_spawn_packet(self);
    }

    async fn process_batch(
        self: &Arc<Self>,
        server: &Arc<Server>,
        payload: Bytes,
    ) -> Result<(), Error> {
        let decoder = *self.network_reader.lock().await;
        if self.is_closed() {
            return Ok(());
        }
        let body = decoder
            .decode_nethernet_batch(payload)
            .map_err(Error::other)?;
        let mut cursor = Cursor::new(body);

        while (cursor.position() as usize) < cursor.get_ref().len() {
            let game_packet = decoder.get_game_packet(&mut cursor).map_err(Error::other)?;

            if !self.packet_limiter.check_packet() {
                warn!(
                    "Bedrock client {} exceeded packet rate limit (rate: {}/s)",
                    self.address,
                    self.packet_limiter.max_rate()
                );
                self.kick(
                    DisconnectReason::Kicked,
                    server
                        .advanced_config
                        .networking
                        .bedrock
                        .packet_limiter
                        .kick_message
                        .clone(),
                )
                .await;
                return Err(Error::other("Packet rate limit exceeded"));
            }

            self.handle_game_packet(game_packet).await?;
        }

        Ok(())
    }

    async fn handle_game_packet(&self, packet: RawPacket) -> Result<(), Error> {
        if let Err(err) = self.incoming_game_packet_send.send(packet).await {
            debug!("Failed to send game packet to session task: {err}");
        }
        Ok(())
    }

    async fn translate_incoming_login(
        &self,
        server: &Arc<Server>,
        packet: RawPacket,
    ) -> Option<RawPacket> {
        if !server
            .plugin_manager
            .has_handlers::<BedrockConnectionPacketReceivedEvent>()
        {
            return Some(packet);
        }
        let mut event = BedrockConnectionPacketReceivedEvent::new(
            self.id,
            self.protocol_version.load(Ordering::Relaxed),
            self.version.load(),
            packet.id,
            packet.payload,
        );
        server.plugin_manager.fire(server, &mut event).await;
        (!event.cancelled).then_some(RawPacket {
            id: event.packet_id,
            payload: event.payload,
        })
    }

    pub async fn handle_login_sequence(
        self: &Arc<Self>,
        server: &Arc<Server>,
    ) -> PacketHandlerResult {
        while let Some(packet) = self.get_packet().await {
            // Pre-1.19.30 clients send SLogin directly without SRequestNetworkSettings.
            if self.protocol_version.load(Ordering::Relaxed) == 0
                && packet.id == SLogin::PACKET_ID
                && let Some(bytes) = packet.payload.get(..4)
                && let Ok(bytes) = <[u8; 4]>::try_from(bytes)
            {
                let client_protocol = i32::from_be_bytes(bytes);
                if !self
                    .admit_legacy_login_protocol(client_protocol, server)
                    .await
                {
                    return PacketHandlerResult::Stop;
                }
            }

            let Some(packet) = self.translate_incoming_login(server, packet).await else {
                continue;
            };
            let payload = &mut Cursor::new(&packet.payload);
            match packet.id {
                SRequestNetworkSettings::PACKET_ID => {
                    let packet = match SRequestNetworkSettings::read(payload) {
                        Ok(p) => p,
                        Err(err) => {
                            error!("Failed to read SRequestNetworkSettings: {err}");
                            continue;
                        }
                    };
                    if !self.handle_request_network_settings(packet, server).await {
                        return PacketHandlerResult::Stop;
                    }
                }
                SLogin::PACKET_ID => {
                    let packet = match SLogin::read(payload) {
                        Ok(p) => p,
                        Err(err) => {
                            error!("Failed to read SLogin: {err}");
                            self.kick(DisconnectReason::BadPacket, err.to_string())
                                .await;
                            return PacketHandlerResult::Stop;
                        }
                    };
                    match self.handle_login(packet, server).await {
                        Ok(result) => return result,
                        Err(err) => {
                            self.kick(DisconnectReason::Unknown, err.to_string()).await;
                            return PacketHandlerResult::Stop;
                        }
                    }
                }
                _ => {
                    debug!(
                        "Received unexpected game packet {} during login sequence",
                        packet.id
                    );
                }
            }
        }
        PacketHandlerResult::Stop
    }

    pub async fn progress_player_packets(self: &Arc<Self>, player: &Arc<Player>) {
        while let Some(packet) = self.get_packet().await {
            self.enqueue_play_input(player, packet);
        }
    }

    pub fn handle_play_packet(
        self: &Arc<Self>,
        player: &Arc<Player>,
        server: &Arc<Server>,
        packet: &RawPacket,
    ) -> Result<(), Error> {
        let mut spin_attack_prediction = None;
        self.handle_queued_play_packet(player, server, packet, &mut spin_attack_prediction, None)?;
        self.reconcile_spin_attack_prediction(player, spin_attack_prediction);
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn handle_queued_play_packet(
        self: &Arc<Self>,
        player: &Arc<Player>,
        server: &Arc<Server>,
        packet: &RawPacket,
        spin_attack_prediction: &mut Option<bool>,
        commands: Option<PlayerCommandIngress<'_>>,
    ) -> Result<(), Error> {
        let payload = &packet.payload[..];
        let reader = &mut &payload[..];
        match packet.id {
            SClientCacheStatus::PACKET_ID => {
                let packet = SClientCacheStatus::read(reader)?;
                self.client_cache_supported
                    .store(packet.is_cache_supported, Ordering::Relaxed);
            }
            SClientCacheBlobStatus::PACKET_ID => {
                let packet = SClientCacheBlobStatus::read(reader)?;
                self.handle_client_cache_blob_status(packet);
            }
            SResourcePackClientResponse::PACKET_ID => {
                let packet = SResourcePackClientResponse::read(reader)?;
                let client = self.clone();
                let server_c = server.clone();
                server.spawn_task(async move {
                    client
                        .handle_resource_pack_response(packet, &server_c)
                        .await;
                });
            }
            SResourcePackChunkRequest::PACKET_ID => {
                let packet = SResourcePackChunkRequest::read(reader)?;
                let client = self.clone();
                let server_c = server.clone();
                server.spawn_task(async move {
                    client
                        .handle_resource_pack_chunk_request(packet, &server_c)
                        .await;
                });
            }
            SPlayerAuthInput::PACKET_ID => {
                let packet = SPlayerAuthInput::read(reader)?;
                self.apply_player_auth_input(player, packet, server, spin_attack_prediction, None);
            }
            SRequestChunkRadius::PACKET_ID => {
                let packet = SRequestChunkRadius::read(reader)?;
                self.handle_request_chunk_radius(player, &packet);
            }
            SInventoryTransaction::PACKET_ID => {
                let packet = SInventoryTransaction::read(reader)?;
                self.handle_inventory_action(player, packet);
            }
            SItemStackRequest::PACKET_ID => {
                let packet = SItemStackRequest::read(reader)?;
                self.handle_item_stack_request(player, packet);
            }
            SInteract::PACKET_ID => {
                let packet = SInteract::read(reader)?;
                self.handle_interaction(player, &packet);
            }
            SContainerClose::PACKET_ID => {
                let packet = SContainerClose::read(reader)?;
                self.handle_container_close(player, &packet);
            }
            SText::PACKET_ID => {
                let text = SText::read(reader)?;
                let client = self.clone();
                let player_c = player.clone();
                let server_c = server.clone();
                player.spawn_task(async move {
                    client.handle_chat_message(&server_c, &player_c, text).await;
                });
            }
            SCommandRequest::PACKET_ID => {
                let req = SCommandRequest::read(reader)?;
                let command = req.command.strip_prefix('/').unwrap_or(&req.command);
                submit_inbound_player_command(server, player, command, commands);
            }
            SSetLocalPlayerAsInitialized::PACKET_ID => {
                self.handle_set_local_player_as_initialized(
                    player,
                    &SSetLocalPlayerAsInitialized::read(reader)?,
                );
            }
            SSetPlayerInventoryOptions::PACKET_ID => {
                let _ = SSetPlayerInventoryOptions::read(reader)?;
                // Ignore for now
            }
            SCursorItemDrag::PACKET_ID => {
                self.handle_cursor_drag(player, &SCursorItemDrag::read(reader)?);
            }
            SStonecutterSetRecipe::PACKET_ID => {
                let packet = SStonecutterSetRecipe::read(reader)?;
                self.handle_stonecutter_set_recipe(player, &packet);
            }
            SMatchmakingCancel::PACKET_ID => {
                let _ = SMatchmakingCancel::read(reader)?;
                // Ignore for now
            }
            SRegisterAudioContent::PACKET_ID => {
                let _ = SRegisterAudioContent::read(reader)?;
                // Ignore for now
            }
            SPlayerAction::PACKET_ID => {
                let packet = SPlayerAction::read(reader)?;
                self.apply_player_action(
                    player,
                    server,
                    packet.action,
                    packet.block_position,
                    spin_attack_prediction,
                );
            }
            SRespawn::PACKET_ID => {
                let packet = SRespawn::read(reader)?;
                self.handle_respawn(player, &packet);
            }
            SAnimate::PACKET_ID => {
                self.handle_animate(player, &SAnimate::read(reader)?);
            }
            SActorEvent::PACKET_ID => {
                self.handle_actor_event(player, &SActorEvent::read(reader)?);
            }
            SEmote::PACKET_ID => {
                self.handle_emote(player, SEmote::read_slice(reader)?);
            }
            SEmoteList::PACKET_ID => {
                self.handle_emote_list(player, &SEmoteList::read(reader)?);
            }
            SModalFormResponse::PACKET_ID => {
                let form_resp = SModalFormResponse::read(reader)?;
                self.handle_modal_form_response(player, server, form_resp);
            }
            // Ignore for now; movement is client authoritative, so there is no prediction to sync
            SLoadingScreen::PACKET_ID | SClientMovementPredictionSync::PACKET_ID => {}
            SBlockPickRequest::PACKET_ID => {
                let packet = SBlockPickRequest::read(reader)?;
                self.handle_block_pick_request(player, &packet);
            }
            SActorPickRequest::PACKET_ID => {
                let packet = SActorPickRequest::read(reader)?;
                self.handle_actor_pick_request(player, &packet);
            }
            SBlockActorData::PACKET_ID => {
                let packet = SBlockActorData::read(reader)?;
                self.handle_block_actor_data(player, &packet);
            }
            SRequestAbility::PACKET_ID => {
                self.handle_request_ability(player, &SRequestAbility::read(reader)?);
            }
            NetworkStackLatency::PACKET_ID => {
                self.handle_network_stack_latency(player, NetworkStackLatency::read(reader)?);
            }
            SMobEquipment::PACKET_ID => {
                let packet = SMobEquipment::read(reader)?;
                self.handle_mob_equipment(server, player, &packet);
            }
            SPlayerHotbar::PACKET_ID => {
                let packet = SPlayerHotbar::read(reader)?;
                self.handle_player_hotbar(server, player, &packet);
            }
            SPacketViolationWarning::PACKET_ID => {
                let warning = SPacketViolationWarning::read(reader)?;
                warn!(
                    violation_type = warning.violation_type.0,
                    violation_severity = warning.violation_severity.0,
                    violation_packet_id = warning.violation_packet_id.0,
                    violation_context = %warning.violation_context,
                    "Bedrock client rejected a server packet"
                );
            }
            id => {
                if let Some(name) = pumpkin_data::bedrock_packet::packet_name(id) {
                    debug!(
                        packet_id = id,
                        name, "Bedrock packet has no gameplay handler"
                    );
                } else {
                    warn!(packet_id = id, "Unknown Bedrock packet");
                }
            }
        }
        Ok(())
    }

    pub fn handle_client_cache_blob_status(&self, packet: SClientCacheBlobStatus) {
        self.apply_client_cache_blob_status(packet, true);
    }

    fn apply_client_cache_blob_status(&self, packet: SClientCacheBlobStatus, observe: bool) {
        let missing_blobs = {
            let mut cache = self
                .blob_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for hash in packet.hit_hashes {
                cache.remove(hash);
            }
            let mut missing_blobs = Vec::with_capacity(packet.miss_hashes.len());
            for hash in packet.miss_hashes {
                if let Some(blob) = cache.get(hash) {
                    missing_blobs.push(MissingBlobData {
                        blob_id: hash,
                        blob_data: blob.clone(),
                    });
                    cache.mark_answered(hash);
                } else {
                    warn!("Client requested missing blob {hash:#x} not found in server cache");
                }
            }
            missing_blobs
        };
        if !missing_blobs.is_empty() {
            let packet = CClientCacheMissResponse { missing_blobs };
            match blob_cache::share_blob_reply(packet) {
                Ok(frame) => {
                    self.try_enqueue_packet_data_inner(frame, observe);
                }
                Err(error) => error!("Failed to serialize Bedrock packet: {error}"),
            }
        }
    }

    pub async fn await_close_interrupt(&self) {
        self.closing.wait().await;
    }

    pub async fn get_packet_payload(&self, packet: Vec<u8>) -> Option<Vec<u8>> {
        let mut network_reader = *self.network_reader.lock().await;
        tokio::select! {
            () = self.await_close_interrupt() => {
                debug!("Canceling player packet processing");
                None
            },
            packet_result = network_reader.get_packet_payload(packet) => {
                match packet_result {
                    Ok(packet) => Some(packet),
                    Err(err) => {
                        if !matches!(err, PacketDecodeError::ConnectionClosed) {
                            debug!("Failed to decode packet from client: {err}");
                            let text = format!("Error while reading incoming packet {err}");
                            self.kick(DisconnectReason::BadPacket, text).await;
                        }
                        None
                    }
                }
            }
        }
    }

    pub fn spawn_task<F>(&self, task: F) -> Option<JoinHandle<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        if self.closing.is_closed() {
            None
        } else {
            Some(self.tasks.spawn_on(task, &self.rt_handle))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_protocol::bedrock::client::biome_definition_list::CBiomeDefinitionList;

    #[tokio::test]
    async fn draining_a_kick_releases_the_peer_and_preserves_a_replacement() {
        let address = "127.0.0.1:0".parse().unwrap();
        let clients = Arc::new(Mutex::new(HashMap::new()));
        let make_client = |id, session| {
            Arc::new(BedrockClient::new(
                id,
                session,
                address,
                clients.clone(),
                PacketRateLimiter::new(false, 0.0, 0.0),
                Weak::new(),
            ))
        };
        let old = make_client(1, NetherNetSession::test_session(address).await);
        let new = make_client(2, NetherNetSession::test_session(address).await);
        clients.lock().await.insert(address, old.clone());
        old.start_outgoing_packet_task();
        old.try_kick(DisconnectReason::Kicked, "leaving".into());
        // On this current-thread runtime the old writer has yet to handle the queued kick.
        clients.lock().await.insert(address, new.clone());
        tokio::time::timeout(std::time::Duration::from_secs(3), old.await_tasks())
            .await
            .unwrap();
        assert!(old.session.is_closed());
        assert!(Arc::ptr_eq(&clients.lock().await[&address], &new));
        assert!(!new.is_closed());
        new.close().await;
        assert!(clients.lock().await.is_empty());
    }

    #[test]
    fn watcher_velocity_waits_only_for_a_remote_untimed_actor() {
        use pumpkin_protocol::{
            bedrock::client::set_actor_motion::CSetActorMotion, codec::var_ulong::VarULong,
        };
        use pumpkin_util::math::vector3::Vector3;

        let mut motion = CSetActorMotion {
            target_runtime_id: VarULong(300),
            motion: Vector3::new(1.0, 2.0, -3.0),
            tick: VarULong(0),
        };
        let mut raw = Vec::new();
        BedrockClient::write_raw_packet(&motion, &mut raw).unwrap();
        let packet = OutgoingPacket::normal(raw.clone().into());
        assert!(ordinary_pose(&packet, Some(12)));
        assert!(!ordinary_pose(&packet, Some(300)));
        assert!(!ordinary_pose(&packet, None));
        assert!(!ordinary_pose(
            &OutgoingPacket::priority(packet.data, oneshot::channel().0),
            Some(12)
        ));
        raw.pop();
        assert!(!ordinary_pose(
            &OutgoingPacket::normal(raw.into()),
            Some(12)
        ));

        motion.tick = VarULong(128);
        let mut timed = Vec::new();
        BedrockClient::write_raw_packet(&motion, &mut timed).unwrap();
        assert!(!ordinary_pose(
            &OutgoingPacket::normal(timed.into()),
            Some(12)
        ));
    }

    #[test]
    fn draining_a_crowded_tick_keeps_frame_size_and_completion_boundaries() {
        let mut packets: Vec<_> = (0..MAX_BATCH_PACKETS)
            .map(|_| OutgoingPacket::normal(Bytes::from_static(&[0; 32])))
            .collect();
        let first = outgoing_frame_len(&packets);
        assert!(first > 64 && first < MAX_BATCH_PACKETS);
        assert_eq!(outgoing_frame_len(&packets[first..]), packets.len() - first);
        packets[3].completion = Some(oneshot::channel().0);
        assert_eq!(outgoing_frame_len(&packets), 3);
        assert_eq!(outgoing_frame_len(&packets[3..]), 1);
    }

    #[test]
    fn raw_packet_framing_roundtrips_high_and_0xfe_packet_ids() {
        for id in [1i32, 143, 254, 300, 416, 1023] {
            let payload = [0xAA, 0xBB, 0xCC];
            let framed = frame_raw_packet(id, &payload).expect("valid packet id");
            let (decoded_id, decoded_payload) =
                split_raw_packet(&framed).expect("split framed packet");
            assert_eq!(i32::from(decoded_id), id);
            assert_eq!(&decoded_payload[..], &payload[..]);
        }
        assert!(frame_raw_packet(-1, &[]).is_none());
        assert!(frame_raw_packet(i32::from(u16::MAX) + 1, &[]).is_none());
    }

    #[test]
    fn write_raw_packet_preserves_multi_byte_packet_ids() {
        let mut buf = Vec::new();
        BedrockClient::write_raw_packet(&CBiomeDefinitionList, &mut buf).unwrap();
        let bytes = Bytes::from(buf);
        let (packet_id, _) = split_raw_packet(&bytes).expect("split CBiomeDefinitionList");
        assert_eq!(i32::from(packet_id), CBiomeDefinitionList::PACKET_ID);
    }
}
