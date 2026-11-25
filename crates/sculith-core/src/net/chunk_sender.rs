use bytes::Bytes;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;
use std::num::NonZero;
use std::sync::{Arc, Weak};
use tracing::warn;

use crate::net::java::chunk_data::CChunkData;
use pumpkin_data::packet::CURRENT_MC_VERSION;
use pumpkin_world::chunk::ChunkData;
use pumpkin_world::cylindrical_chunk_iterator::Cylindrical;
use pumpkin_world::level::{Level, SyncChunk};
use sculith_protocol::codec::var_int::VarInt;
use sculith_protocol::java::client::play::{CChunkBatchEnd, CChunkBatchStart, CUnloadChunk};
use sculith_protocol::ser::NetworkWriteExt;
use sculith_protocol::{ClientPacket, MAX_PACKET_DATA_SIZE, MultiVersionJavaPacket};
use sculith_util::math::vector2::Vector2;
use sculith_util::version::JavaMinecraftVersion;

use crate::net::{ClientPlatform, java::JavaClient};

const MIN_CHUNKS_PER_TICK: f32 = 0.01;
const MAX_CHUNKS_PER_TICK: f32 = 64.0;
const INITIAL_CHUNKS_PER_TICK: f32 = 9.0;
const MAX_CONCURRENT_BATCHES: u16 = 10;
/// Encode attempts per chunk before a Bedrock dispatch is given up on.
const MAX_ENCODE_ATTEMPTS: u8 = 3;
/// Ticks a chunk cancelled by `ChunkSend` waits before it is offered again.
const CANCELLED_RETRY_TICKS: u64 = 20;

pub struct PreparedChunk {
    pub position: Vector2<i32>,
    pub chunk: SyncChunk,
}

/// A committed Bedrock chunk plus the token identifying this dispatch of it.
pub struct DispatchedChunk {
    pub position: Vector2<i32>,
    pub chunk: SyncChunk,
    pub delivery_token: u64,
}

/// Why a committed Bedrock chunk did not reach the client.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeliveryFailure {
    /// Cancelled `ChunkSend` -> parked, offered again after `CANCELLED_RETRY_TICKS`.
    Cancelled,
    /// Not queued for a passing reason (player not set, closed connection) -> retried.
    Deferred,
    /// Failed to encode -> retried up to `MAX_ENCODE_ATTEMPTS` times.
    Unencodable,
}

/// Result of a client's `send_chunks`, the same for Java and Bedrock.
#[derive(Default)]
pub struct ChunkSendOutcome {
    pub sent: Vec<Vector2<i32>>,
    pub failed: Vec<(Vector2<i32>, DeliveryFailure)>,
}

impl ChunkSendOutcome {
    /// A chunk with no reported outcome was never attempted -> deferred.
    #[must_use]
    pub fn failure(&self, pos: Vector2<i32>) -> DeliveryFailure {
        self.failed
            .iter()
            .find(|(failed, _)| *failed == pos)
            .map_or(DeliveryFailure::Deferred, |&(_, failure)| failure)
    }

    /// Marks every chunk a `ChunkSend` handler filtered out of `chunks` as cancelled.
    pub fn record_cancelled(&mut self, chunks: &[SyncChunk], valid: &[SyncChunk]) {
        self.failed.extend(
            chunks
                .iter()
                .filter(|chunk| !valid.iter().any(|kept| Arc::ptr_eq(kept, chunk)))
                .map(|chunk| (Vector2::new(chunk.x, chunk.z), DeliveryFailure::Cancelled)),
        );
    }
}

pub struct PreparedBatch {
    pub chunks: Vec<PreparedChunk>,
    pub epoch_snapshot: u32,
    /// Client protocol, only for batch flow -> chunks are always encoded as `CURRENT_MC_VERSION`.
    pub target_version: JavaMinecraftVersion,
}

#[derive(Clone)]
pub struct EncodedChunk {
    pub position: Vector2<i32>,
    pub payload: Bytes,
    pub chunk_ref: Weak<ChunkData>,
    terrain_revision: (u64, u64),
}

impl EncodedChunk {
    fn encode(candidate: &PreparedChunk) -> Option<Self> {
        let chunk = &candidate.chunk;
        let terrain_revision = chunk.section.terrain_revision();
        let mut buffer = Vec::with_capacity(32 * 1024);
        buffer
            .write_var_int(&VarInt(CChunkData::to_id(CURRENT_MC_VERSION)))
            .ok()?;
        CChunkData(chunk)
            .write_packet_data(&mut buffer, &CURRENT_MC_VERSION)
            .ok()?;
        Some(Self {
            position: candidate.position,
            payload: Bytes::from(buffer),
            chunk_ref: Arc::downgrade(chunk),
            terrain_revision,
        })
    }

    #[must_use]
    pub fn is_fresh_for(&self, candidate: &PreparedChunk) -> bool {
        let Some(held) = self.chunk_ref.upgrade() else {
            return false;
        };

        self.position == candidate.position
            && Arc::ptr_eq(&held, &candidate.chunk)
            && held.section.terrain_revision() == self.terrain_revision
    }

    fn has_current_terrain(&self) -> bool {
        self.chunk_ref
            .upgrade()
            .is_some_and(|chunk| chunk.section.terrain_revision() == self.terrain_revision)
    }
}

/// A committed Bedrock dispatch whose packet is still being encoded.
#[derive(Debug)]
struct AwaitingDelivery {
    token: u64,
    /// The client already held a copy before this dispatch.
    held_before: bool,
    /// Superseded dispatches of the position that may still queue the chunk.
    earlier: Vec<u64>,
}

#[derive(Debug)]
pub struct ChunkSender {
    pub pending_chunks: FxHashSet<Vector2<i32>>,
    sent_chunks: FxHashSet<Vector2<i32>>,
    /// Committed Bedrock chunks whose packets are still being encoded.
    awaiting_delivery: FxHashMap<Vector2<i32>, AwaitingDelivery>,
    /// Failed Bedrock encode attempts per chunk.
    encode_failures: FxHashMap<Vector2<i32>, u8>,
    /// Chunks cancelled by `ChunkSend`, by the tick they become pending again.
    /// Kept out of `pending_chunks` so they take no batch slots meanwhile.
    parked_chunks: FxHashMap<Vector2<i32>, u64>,
    /// `parked_chunks` entries in release order; stale ones are skipped.
    park_queue: VecDeque<(u64, Vector2<i32>)>,
    /// Counts `prepare_batch` calls.
    batch_tick: u64,
    /// Monotonic across resets, so a superseded dispatch can never match again.
    next_delivery_token: u64,
    pub in_flight_batches: u16,
    pub desired_rate: f32,
    pub send_quota: f32,
    pub max_in_flight: u16,
}

impl ChunkSender {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending_chunks: FxHashSet::default(),
            sent_chunks: FxHashSet::default(),
            awaiting_delivery: FxHashMap::default(),
            encode_failures: FxHashMap::default(),
            parked_chunks: FxHashMap::default(),
            park_queue: VecDeque::new(),
            batch_tick: 0,
            next_delivery_token: 0,
            in_flight_batches: 0,
            desired_rate: INITIAL_CHUNKS_PER_TICK,
            send_quota: 0.0,
            max_in_flight: 1,
        }
    }

    pub fn reset(&mut self) {
        self.pending_chunks.clear();
        self.sent_chunks.clear();
        self.awaiting_delivery.clear();
        self.encode_failures.clear();
        self.parked_chunks.clear();
        self.park_queue.clear();
        self.in_flight_batches = 0;
        self.send_quota = 0.0;
    }

    #[must_use]
    pub fn is_chunk_sent(&self, pos: &Vector2<i32>) -> bool {
        self.sent_chunks.contains(pos)
    }

    /// Vanilla `ChunkMap.isChunkTracked` -> the client holds this chunk (view checked by caller).
    #[must_use]
    pub fn is_chunk_ready(&self, pos: &Vector2<i32>) -> bool {
        self.sent_chunks.contains(pos) && !self.awaiting_delivery.contains_key(pos)
    }

    /// Bedrock chunks become ready once `send_chunks` has queued them. Returns the
    /// positions that became ready: a position re-enqueued since its dispatch holds a
    /// newer token and stays not ready until that dispatch completes.
    pub fn mark_delivered(&mut self, deliveries: &[(Vector2<i32>, u64)]) -> Vec<Vector2<i32>> {
        let mut delivered = Vec::with_capacity(deliveries.len());
        for &(pos, token) in deliveries {
            let Some(awaiting) = self.awaiting_delivery.get_mut(&pos) else {
                continue;
            };
            if awaiting.token == token {
                self.awaiting_delivery.remove(&pos);
                self.encode_failures.remove(&pos);
                delivered.push(pos);
            } else if let Some(i) = awaiting.earlier.iter().position(|&t| t == token) {
                // A superseded dispatch queued the chunk, so the client holds a copy now
                awaiting.earlier.swap_remove(i);
                awaiting.held_before = true;
            }
        }
        delivered
    }

    /// Cancelled `ChunkSend` before dispatch -> the client never gets it, so it is parked.
    pub fn cancel_pending(
        &mut self,
        batch: &PreparedBatch,
        current_epoch: u32,
        cancelled: &[Vector2<i32>],
    ) {
        if current_epoch != batch.epoch_snapshot {
            return;
        }
        for &pos in cancelled {
            if self.pending_chunks.remove(&pos) {
                self.park(pos);
            }
        }
    }

    /// Holds a cancelled chunk back from batches until its retry tick.
    fn park(&mut self, pos: Vector2<i32>) {
        self.pending_chunks.remove(&pos);
        let release_tick = self.batch_tick + CANCELLED_RETRY_TICKS;
        self.parked_chunks.insert(pos, release_tick);
        self.park_queue.push_back((release_tick, pos));
    }

    /// Moves parked chunks whose retry tick has come back to pending.
    fn release_parked(&mut self) {
        while let Some(&(release_tick, pos)) = self.park_queue.front() {
            if release_tick > self.batch_tick {
                break;
            }
            self.park_queue.pop_front();
            if self.parked_chunks.get(&pos) == Some(&release_tick) {
                self.parked_chunks.remove(&pos);
                self.pending_chunks.insert(pos);
            }
        }
    }

    /// Bedrock dispatch that never reached the client -> no longer held, unless an earlier
    /// copy was, and pending again if retryable. A re-enqueue since then holds a newer token
    /// and is kept. Returns the positions that are ready again because the held copy is
    /// no longer awaiting a dispatch.
    pub fn fail_delivery(
        &mut self,
        failed: &[(Vector2<i32>, u64, DeliveryFailure)],
    ) -> Vec<Vector2<i32>> {
        let mut ready = Vec::new();
        for &(pos, token, failure) in failed {
            let Some(awaiting) = self.awaiting_delivery.get_mut(&pos) else {
                continue;
            };
            if awaiting.token != token {
                // A superseded dispatch failed; the newer one decides the outcome
                if let Some(i) = awaiting.earlier.iter().position(|&t| t == token) {
                    awaiting.earlier.swap_remove(i);
                }
                continue;
            }
            if let Some(earlier) = awaiting.earlier.pop() {
                // An earlier dispatch could still queue the chunk, so it stays sent
                awaiting.token = earlier;
            } else {
                let held_before = awaiting.held_before;
                self.awaiting_delivery.remove(&pos);
                if held_before {
                    ready.push(pos);
                } else {
                    self.sent_chunks.remove(&pos);
                }
            }

            let retry = match failure {
                DeliveryFailure::Cancelled => {
                    self.park(pos);
                    continue;
                }
                DeliveryFailure::Deferred => true,
                DeliveryFailure::Unencodable => self.record_encode_failure(pos),
            };
            if retry {
                self.pending_chunks.insert(pos);
            }
        }
        ready
    }

    /// Counts a failed encode. `false` once the chunk is given up on.
    fn record_encode_failure(&mut self, pos: Vector2<i32>) -> bool {
        let attempts = self.encode_failures.entry(pos).or_default();
        *attempts += 1;
        if *attempts < MAX_ENCODE_ATTEMPTS {
            return true;
        }
        self.encode_failures.remove(&pos);
        warn!("Giving up on chunk {pos:?} after {MAX_ENCODE_ATTEMPTS} encode attempts");
        false
    }

    #[must_use]
    pub fn sent_chunks_count(&self) -> usize {
        self.sent_chunks.len()
    }

    /// Chunks the client holds, without Bedrock dispatches still being queued.
    #[must_use]
    pub fn ready_chunks_count(&self) -> usize {
        self.sent_chunks
            .len()
            .saturating_sub(self.awaiting_delivery.len())
    }

    /// Keep at most one round of terrain work pending for this Bedrock viewer.
    #[must_use]
    pub fn has_pending_bedrock_delivery(&self) -> bool {
        !self.awaiting_delivery.is_empty()
    }

    pub(crate) fn has_bedrock_chunk_work(&self) -> bool {
        self.has_pending_bedrock_delivery()
            || !self.pending_chunks.is_empty()
            || !self.parked_chunks.is_empty()
    }

    /// Records a chunk sent outside the batch path as held.
    pub fn mark_sent_out_of_band(&mut self, pos: Vector2<i32>) {
        self.pending_chunks.remove(&pos);
        self.parked_chunks.remove(&pos);
        self.awaiting_delivery.remove(&pos);
        self.encode_failures.remove(&pos);
        self.sent_chunks.insert(pos);
    }

    pub const fn on_batch_acknowledged(&mut self, client_requested_rate: f32) -> bool {
        if self.in_flight_batches == 0 {
            return false;
        }

        self.in_flight_batches = self.in_flight_batches.saturating_sub(1);
        self.desired_rate = if client_requested_rate.is_nan() {
            MIN_CHUNKS_PER_TICK
        } else {
            client_requested_rate.clamp(MIN_CHUNKS_PER_TICK, MAX_CHUNKS_PER_TICK)
        };

        if self.in_flight_batches == 0 {
            self.send_quota = 1.0;
        }

        self.max_in_flight = MAX_CONCURRENT_BATCHES;
        true
    }

    /// Vanilla `ChunkMap.markChunkPendingToSend` -> a held copy stays tracked while re-queued.
    pub fn enqueue_chunk(&mut self, pos: Vector2<i32>) {
        // A Bedrock dispatch still encoding keeps its marker until it resolves
        self.parked_chunks.remove(&pos);
        self.pending_chunks.insert(pos);
    }

    pub fn unload_chunk(&mut self, client: &ClientPlatform, pos: Vector2<i32>) {
        self.pending_chunks.remove(&pos);
        self.awaiting_delivery.remove(&pos);
        self.encode_failures.remove(&pos);
        self.parked_chunks.remove(&pos);
        if self.sent_chunks.remove(&pos)
            && let ClientPlatform::Java(java_client) = client
            && !java_client.is_closed()
        {
            java_client.try_send_packet(&CUnloadChunk::new(pos.x, pos.y));
        }
    }

    fn collect_sorted_candidates(
        &self,
        loaded: impl Fn(Vector2<i32>) -> Option<SyncChunk>,
        center: Vector2<i32>,
        view_distance: NonZero<u8>,
        max_chunks: usize,
    ) -> Vec<PreparedChunk> {
        let quota_limit = (self.send_quota.floor() as usize).min(max_chunks);
        let mut ready = Vec::with_capacity(quota_limit);

        // If pending_chunks is small, sorting it directly avoids scanning offsets.
        if self.pending_chunks.len() <= 16 {
            let mut sorted: Vec<Vector2<i32>> = self.pending_chunks.iter().copied().collect();
            sorted.sort_unstable_by_key(|pos| {
                let dx = (pos.x - center.x).unsigned_abs() as u64;
                let dz = (pos.y - center.y).unsigned_abs() as u64;
                dx * dx + dz * dz
            });

            for pos in sorted {
                if ready.len() >= quota_limit {
                    break;
                }

                if let Some(chunk) = loaded(pos) {
                    ready.push(PreparedChunk {
                        position: pos,
                        chunk,
                    });
                }
            }
        } else {
            // Re-use precompiled cylindrical chunk view LUT which is already sorted center-outward.
            let offsets = Cylindrical::get_offsets(view_distance.get());
            for &(dx, dy) in offsets {
                if ready.len() >= quota_limit {
                    break;
                }

                let pos = Vector2::new(center.x + i32::from(dx), center.y + i32::from(dy));
                if self.pending_chunks.contains(&pos)
                    && let Some(chunk) = loaded(pos)
                {
                    ready.push(PreparedChunk {
                        position: pos,
                        chunk,
                    });
                }
            }

            // Fallback for any pending chunks outside the precomputed table
            if ready.is_empty() {
                for &pos in &self.pending_chunks {
                    if ready.len() >= quota_limit {
                        break;
                    }
                    if let Some(chunk) = loaded(pos) {
                        ready.push(PreparedChunk {
                            position: pos,
                            chunk,
                        });
                    }
                }
            }
        }

        ready
    }

    pub fn prepare_batch(
        &mut self,
        level: &Level,
        player_chunk: Vector2<i32>,
        view_distance: NonZero<u8>,
        epoch: u32,
        version: JavaMinecraftVersion,
        max_chunks: usize,
    ) -> Option<PreparedBatch> {
        self.prepare_batch_with(
            player_chunk,
            view_distance,
            epoch,
            version,
            max_chunks,
            |pos| {
                level
                    .loaded_chunks
                    .get(&pos)
                    .map(|chunk| chunk.value().clone())
            },
        )
    }

    pub(crate) fn prepare_regional_bedrock_batch(
        &mut self,
        level: &Level,
        player_chunk: Vector2<i32>,
        view: Cylindrical,
        epoch: u32,
        max_chunks: usize,
    ) -> Option<PreparedBatch> {
        self.prepare_batch_with(
            player_chunk,
            view.view_distance,
            epoch,
            JavaMinecraftVersion::V_1_20_2,
            max_chunks,
            |pos| {
                if !view.is_within_distance(pos.x, pos.y) {
                    return None;
                }
                level
                    .loaded_chunks
                    .try_get(&pos)
                    .try_unwrap()
                    .map(|chunk| chunk.value().clone())
            },
        )
    }

    fn prepare_batch_with(
        &mut self,
        player_chunk: Vector2<i32>,
        view_distance: NonZero<u8>,
        epoch: u32,
        version: JavaMinecraftVersion,
        max_chunks: usize,
        loaded: impl Fn(Vector2<i32>) -> Option<SyncChunk>,
    ) -> Option<PreparedBatch> {
        self.batch_tick += 1;
        self.release_parked();

        if max_chunks == 0
            || version >= JavaMinecraftVersion::V_1_20_2
                && self.in_flight_batches >= self.max_in_flight
        {
            return None;
        }

        let max_batch = self.desired_rate.max(1.0);
        self.send_quota = (self.send_quota + self.desired_rate).min(max_batch);

        if self.send_quota < 1.0 || self.pending_chunks.is_empty() {
            return None;
        }

        let candidates =
            self.collect_sorted_candidates(loaded, player_chunk, view_distance, max_chunks);
        if candidates.is_empty() {
            return None;
        }

        Some(PreparedBatch {
            chunks: candidates,
            epoch_snapshot: epoch,
            target_version: version,
        })
    }

    pub fn encode_batch(
        batch: &PreparedBatch,
        cache: &mut FxHashMap<Vector2<i32>, EncodedChunk>,
    ) -> Vec<EncodedChunk> {
        let cached_map = &*cache;

        let encoded_results: Vec<Option<EncodedChunk>> = batch
            .chunks
            .par_iter()
            .map(|candidate| {
                let pos = candidate.position;
                if let Some(cached) = cached_map.get(&pos)
                    && cached.is_fresh_for(candidate)
                {
                    return Some(cached.clone());
                }

                EncodedChunk::encode(candidate)
            })
            .collect();

        let mut output = Vec::with_capacity(encoded_results.len());
        for encoded in encoded_results.into_iter().flatten() {
            cache.insert(encoded.position, encoded.clone());
            output.push(encoded);
        }

        output
    }

    #[must_use]
    pub fn encode_batch_shared(
        batch: &PreparedBatch,
        cache: &dashmap::DashMap<Vector2<i32>, EncodedChunk>,
    ) -> Vec<EncodedChunk> {
        Self::encode_current_batch_shared(batch, cache, || true).unwrap_or_default()
    }

    pub(crate) fn encode_current_batch_shared(
        batch: &PreparedBatch,
        cache: &dashmap::DashMap<Vector2<i32>, EncodedChunk>,
        current: impl Fn() -> bool + Sync,
    ) -> Option<Vec<EncodedChunk>> {
        if !current() {
            return None;
        }
        let encoded = batch
            .chunks
            .par_iter()
            .filter_map(|candidate| {
                if !current() {
                    return None;
                }
                let pos = candidate.position;
                if let Some(cached) = cache.get(&pos)
                    && cached.is_fresh_for(candidate)
                {
                    return Some(cached.value().clone());
                }

                let encoded = EncodedChunk::encode(candidate)?;
                if !current() {
                    return None;
                }
                cache.insert(pos, encoded.clone());
                Some(encoded)
            })
            .collect();
        current().then_some(encoded)
    }

    /// Chunks `encode_batch` dropped stay pending, retried a bounded number of times.
    fn settle_unencoded(&mut self, batch: &PreparedBatch, encoded_chunks: &[EncodedChunk]) {
        for candidate in &batch.chunks {
            let pos = candidate.position;
            if self.pending_chunks.contains(&pos)
                && !encoded_chunks.iter().any(|encoded| encoded.position == pos)
                && !self.record_encode_failure(pos)
            {
                self.pending_chunks.remove(&pos);
            }
        }
    }

    /// Commit a current Java batch when its packets need no outgoing observers or translation.
    pub(crate) fn commit_current_java_batch(
        &mut self,
        batch: &PreparedBatch,
        encoded_chunks: &[EncodedChunk],
        client: &JavaClient,
        current_epoch: u32,
    ) -> Vec<Vector2<i32>> {
        if current_epoch != batch.epoch_snapshot {
            return Vec::new();
        }
        self.settle_unencoded(batch, encoded_chunks);
        if encoded_chunks.is_empty() {
            return Vec::new();
        }

        let Ok(start) = client.serialize_packet(&CChunkBatchStart) else {
            return Vec::new();
        };
        let Ok(largest_end) =
            client.serialize_packet(&CChunkBatchEnd::new(encoded_chunks.len() as u16))
        else {
            return Vec::new();
        };
        let mut bytes = client
            .terrain_queue_room()
            .saturating_sub(start.len() + largest_end.len());
        let mut selected = Vec::with_capacity(encoded_chunks.len());
        for chunk in encoded_chunks {
            if !self.pending_chunks.contains(&chunk.position) || !chunk.has_current_terrain() {
                continue;
            }
            if chunk.payload.len() > MAX_PACKET_DATA_SIZE {
                if !self.record_encode_failure(chunk.position) {
                    self.pending_chunks.remove(&chunk.position);
                }
                continue;
            }
            let Some(remaining) = bytes.checked_sub(chunk.payload.len()) else {
                if selected.is_empty() {
                    tracing::trace!(
                        connection = client.id,
                        available_payload_bytes = bytes,
                        required_payload_bytes = chunk.payload.len(),
                        "Deferred Java terrain until the writer has room"
                    );
                }
                break;
            };
            bytes = remaining;
            selected.push(chunk);
        }
        if selected.is_empty() {
            return Vec::new();
        }
        let Ok(end) = client.serialize_packet(&CChunkBatchEnd::new(selected.len() as u16)) else {
            return Vec::new();
        };
        let mut packets = Vec::with_capacity(selected.len() + 2);
        packets.push(start);
        packets.extend(selected.iter().map(|chunk| chunk.payload.clone()));
        packets.push(end);
        if selected.iter().any(|chunk| !chunk.has_current_terrain())
            || !client.try_enqueue_current_terrain_batch(&packets)
        {
            return Vec::new();
        }
        let sent = selected
            .iter()
            .map(|chunk| chunk.position)
            .collect::<Vec<_>>();
        for &pos in &sent {
            self.mark_sent_out_of_band(pos);
        }
        self.in_flight_batches = self.in_flight_batches.saturating_add(1);
        self.send_quota -= sent.len() as f32;
        sent
    }

    /// Translated and observed batches preserve the per-packet send path.
    pub fn commit_batch(
        &mut self,
        batch: &PreparedBatch,
        encoded_chunks: &[EncodedChunk],
        client: &ClientPlatform,
        current_epoch: u32,
    ) -> Vec<Vector2<i32>> {
        if current_epoch != batch.epoch_snapshot {
            return Vec::new();
        }

        self.settle_unencoded(batch, encoded_chunks);
        if encoded_chunks.is_empty() {
            return Vec::new();
        }

        let mut dispatched_positions = Vec::with_capacity(encoded_chunks.len());
        let version = batch.target_version;

        let batch_client = match client {
            ClientPlatform::Java(java_client) if version >= JavaMinecraftVersion::V_1_20_2 => {
                // Without the start packet the client would get an end packet out of nowhere
                if !java_client.try_send_packet(&CChunkBatchStart) {
                    return Vec::new();
                }
                Some(java_client)
            }
            _ => None,
        };

        for chunk in encoded_chunks {
            // A regional commit can change a palette while it is being encoded.
            // A stale result leaves the pending chunk in place for another batch.
            if !chunk.has_current_terrain() || !self.pending_chunks.remove(&chunk.position) {
                continue;
            }

            // A dropped packet (e.g. cancelled `PacketSentEvent`) never reached the client.
            let queued = match client {
                ClientPlatform::Java(java_client) => {
                    java_client.try_enqueue_packet_data(chunk.payload.clone())
                }
                ClientPlatform::Bedrock(_) => {
                    client.try_enqueue_packet(chunk.payload.clone());
                    true
                }
            };
            if queued {
                self.encode_failures.remove(&chunk.position);
                self.sent_chunks.insert(chunk.position);
                dispatched_positions.push(chunk.position);
            } else {
                // Dropped on purpose (e.g. a `PacketSentEvent` cancel) -> offered again later
                self.park(chunk.position);
            }
        }

        let sent_count = dispatched_positions.len();
        // A started batch is always closed. The client acks every `CChunkBatchEnd` it receives,
        // so only a queued one counts as in flight
        if let Some(java_client) = batch_client
            && java_client.try_send_packet(&CChunkBatchEnd::new(sent_count as u16))
        {
            self.in_flight_batches = self.in_flight_batches.saturating_add(1);
        }
        self.send_quota -= sent_count as f32;

        dispatched_positions
    }

    /// Marks a prepared Bedrock batch as dispatched and returns its chunks for encoding.
    ///
    /// Bedrock chunks use a different encoder from Java chunks, but they must still move from
    /// `pending_chunks` to `sent_chunks`. Otherwise the same batch is selected every tick and the
    /// Bedrock login flow never reaches its minimum-chunk spawn threshold.
    /// They stay not ready until [`Self::mark_delivered`].
    pub fn commit_bedrock_batch(
        &mut self,
        batch: &PreparedBatch,
        current_epoch: u32,
    ) -> Vec<DispatchedChunk> {
        if current_epoch != batch.epoch_snapshot || batch.chunks.is_empty() {
            return Vec::new();
        }

        let mut dispatched_chunks = Vec::with_capacity(batch.chunks.len());
        for candidate in &batch.chunks {
            if !self.pending_chunks.remove(&candidate.position) {
                continue;
            }

            let delivery_token = self.next_delivery_token;
            self.next_delivery_token += 1;
            let (held_before, earlier) = match self.awaiting_delivery.remove(&candidate.position) {
                Some(mut previous) => {
                    previous.earlier.push(previous.token);
                    (previous.held_before, previous.earlier)
                }
                None => (self.sent_chunks.contains(&candidate.position), Vec::new()),
            };
            self.sent_chunks.insert(candidate.position);
            self.awaiting_delivery.insert(
                candidate.position,
                AwaitingDelivery {
                    token: delivery_token,
                    held_before,
                    earlier,
                },
            );
            dispatched_chunks.push(DispatchedChunk {
                position: candidate.position,
                chunk: candidate.chunk.clone(),
                delivery_token,
            });
        }

        self.send_quota -= dispatched_chunks.len() as f32;
        dispatched_chunks
    }
}

impl Default for ChunkSender {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{
        GameProfile, PacketRateLimiter, PlayerConfig, java::pending::PendingConnection,
    };
    use arc_swap::ArcSwap;
    use pumpkin_data::Block;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn java_terrain_defers_without_losing_pending_chunks() {
        let pending = PendingConnection::from_transport(
            Box::new(tokio::io::empty()),
            Box::new(tokio::io::sink()),
            "127.0.0.1:25565".parse().unwrap(),
            0,
            PacketRateLimiter::new(false, 0.0, 0.0),
            Weak::new(),
        );
        let client = JavaClient::from_pending(
            pending,
            GameProfile {
                id: uuid::Uuid::nil(),
                name: "TerrainBudget".into(),
                properties: ArcSwap::from_pointee(Vec::new()),
                profile_actions: None,
            },
            PlayerConfig::default(),
        );
        let first = Vector2::new(0, 0);
        let second = Vector2::new(1, 0);
        let mut batch = batch_for(first, 0);
        batch.target_version = CURRENT_MC_VERSION;
        batch.chunks.push(batch_for(second, 0).chunks.remove(0));
        let encoded = ChunkSender::encode_batch(&batch, &mut FxHashMap::default());
        let mut sender = ChunkSender::new();
        sender.enqueue_chunk(first);
        sender.enqueue_chunk(second);
        sender.send_quota = 2.0;

        let budget = client.terrain_queue_room();
        let chunk_bytes = encoded[0].payload.len();
        let headers = client.serialize_packet(&CChunkBatchStart).unwrap().len()
            + client
                .serialize_packet(&CChunkBatchEnd::new(1))
                .unwrap()
                .len();
        client
            .pending_bytes
            .store(budget - chunk_bytes, Ordering::Release);
        assert!(
            sender
                .commit_current_java_batch(&batch, &encoded, &client, 0)
                .is_empty()
        );
        assert!(!client.is_closed());
        assert_eq!(sender.in_flight_batches, 0);
        assert_eq!(sender.pending_chunks.len(), 2);
        assert_eq!(sender.send_quota, 2.0);

        client
            .pending_bytes
            .store(budget - chunk_bytes - headers, Ordering::Release);
        assert_eq!(
            sender.commit_current_java_batch(&batch, &encoded, &client, 0),
            [first]
        );
        assert_eq!(client.terrain_queue_room(), 0);
        assert_eq!(sender.in_flight_batches, 1);
        assert!(sender.pending_chunks.contains(&second));
        assert!(sender.is_chunk_sent(&first));

        client.pending_bytes.store(0, Ordering::Release);
        assert_eq!(
            sender.commit_current_java_batch(&batch, &encoded, &client, 0),
            [second]
        );
        assert_eq!(sender.send_quota, 0.0);
        assert!(sender.is_chunk_sent(&second));
    }

    #[test]
    fn java_chunk_encoding_expires_when_the_same_chunk_changes() {
        let position = Vector2::new(3, -2);
        let batch = batch_for(position, 0);
        let candidate = &batch.chunks[0];
        let mut cache = FxHashMap::default();
        let encoded = ChunkSender::encode_batch(&batch, &mut cache);
        assert!(encoded[0].is_fresh_for(candidate));

        candidate
            .chunk
            .set_block_absolute_y(0, 64, 0, Block::STONE.default_state.id);
        assert!(!encoded[0].has_current_terrain());
        assert!(!encoded[0].is_fresh_for(candidate));

        let current = ChunkSender::encode_batch(&batch, &mut cache);
        assert!(current[0].is_fresh_for(candidate));
        assert_ne!(encoded[0].payload, current[0].payload);
    }

    #[test]
    fn bedrock_batch_moves_pending_chunks_to_sent() {
        let position = Vector2::new(3, -2);
        let chunk = ChunkData::empty_sync(position.x, position.y);
        let batch = PreparedBatch {
            chunks: vec![PreparedChunk {
                position,
                chunk: chunk.clone(),
            }],
            epoch_snapshot: 7,
            target_version: JavaMinecraftVersion::V_1_20_2,
        };
        let mut sender = ChunkSender::new();
        sender.enqueue_chunk(position);
        sender.send_quota = 1.0;

        let dispatched = sender.commit_bedrock_batch(&batch, 7);

        assert_eq!(dispatched.len(), 1);
        assert!(Arc::ptr_eq(&dispatched[0].chunk, &chunk));
        assert!(!sender.pending_chunks.contains(&position));
        assert!(sender.is_chunk_sent(&position));
        assert_eq!(sender.sent_chunks_count(), 1);
        assert_eq!(sender.send_quota, 0.0);

        let repeated = sender.commit_bedrock_batch(&batch, 7);

        assert!(repeated.is_empty());
        assert_eq!(sender.sent_chunks_count(), 1);
        assert_eq!(sender.send_quota, 0.0);
    }

    #[test]
    fn bedrock_batch_ignores_a_stale_epoch() {
        let position = Vector2::new(3, -2);
        let batch = PreparedBatch {
            chunks: vec![PreparedChunk {
                position,
                chunk: ChunkData::empty_sync(position.x, position.y),
            }],
            epoch_snapshot: 7,
            target_version: JavaMinecraftVersion::V_1_20_2,
        };
        let mut sender = ChunkSender::new();
        sender.enqueue_chunk(position);

        let dispatched = sender.commit_bedrock_batch(&batch, 8);

        assert!(dispatched.is_empty());
        assert!(sender.pending_chunks.contains(&position));
        assert_eq!(sender.sent_chunks_count(), 0);
    }

    #[test]
    fn watchers_become_ready_on_their_own_delivery() {
        let position = Vector2::new(1, 4);
        let batch = PreparedBatch {
            chunks: vec![PreparedChunk {
                position,
                chunk: ChunkData::empty_sync(position.x, position.y),
            }],
            epoch_snapshot: 0,
            target_version: JavaMinecraftVersion::V_1_20_2,
        };
        let mut first = ChunkSender::new();
        let mut second = ChunkSender::new();
        first.enqueue_chunk(position);
        second.enqueue_chunk(position);
        assert!(!first.is_chunk_ready(&position));
        assert!(!second.is_chunk_ready(&position));

        let first_dispatch = first.commit_bedrock_batch(&batch, 0);
        assert!(first.has_pending_bedrock_delivery());
        assert!(!second.has_pending_bedrock_delivery());
        assert!(!first.is_chunk_ready(&position));
        first.mark_delivered(&[(position, first_dispatch[0].delivery_token)]);
        assert!(!first.has_pending_bedrock_delivery());
        assert!(first.is_chunk_ready(&position));
        assert!(!second.is_chunk_ready(&position));

        let second_dispatch = second.commit_bedrock_batch(&batch, 0);
        assert!(!second.is_chunk_ready(&position));
        second.mark_delivered(&[(position, second_dispatch[0].delivery_token)]);
        assert!(second.is_chunk_ready(&position));

        // Re-queueing a held chunk keeps it tracked.
        first.enqueue_chunk(position);
        assert!(first.is_chunk_ready(&position));
        assert!(second.is_chunk_ready(&position));
    }

    #[test]
    fn stale_bedrock_delivery_cannot_clear_a_newer_batchs_marker() {
        // A world change resets the sender and re-sends the same relative position
        // under a new epoch while the old dispatch is still awaiting send_chunks.
        let position = Vector2::new(5, -1);
        let mut sender = ChunkSender::new();

        sender.enqueue_chunk(position);
        let stale = sender.commit_bedrock_batch(&batch_for(position, 0), 0);

        sender.reset();
        sender.enqueue_chunk(position);
        sender.commit_bedrock_batch(&batch_for(position, 1), 1);
        assert!(!sender.is_chunk_ready(&position));

        let delivered = sender.mark_delivered(&[(position, stale[0].delivery_token)]);

        assert!(delivered.is_empty());
        assert!(!sender.is_chunk_ready(&position));
    }

    #[test]
    fn same_epoch_re_enqueue_keeps_its_own_delivery_marker() {
        // Re-enqueueing within one epoch dispatches the position again while the
        // first task is still awaiting send_chunks.
        let position = Vector2::new(-3, 8);
        let mut sender = ChunkSender::new();

        sender.enqueue_chunk(position);
        let first = sender.commit_bedrock_batch(&batch_for(position, 0), 0);

        sender.enqueue_chunk(position);
        let second = sender.commit_bedrock_batch(&batch_for(position, 0), 0);
        assert_ne!(first[0].delivery_token, second[0].delivery_token);
        assert!(!sender.is_chunk_ready(&position));

        // First task finishes last: its token is superseded, nothing clears.
        let delivered = sender.mark_delivered(&[(position, first[0].delivery_token)]);
        assert!(delivered.is_empty());
        assert!(!sender.is_chunk_ready(&position));

        let delivered = sender.mark_delivered(&[(position, second[0].delivery_token)]);
        assert_eq!(delivered, vec![position]);
        assert!(sender.is_chunk_ready(&position));
    }

    #[test]
    fn re_enqueue_during_dispatch_is_not_counted_ready() {
        let position = Vector2::new(2, 2);
        let mut sender = ChunkSender::new();

        sender.enqueue_chunk(position);
        let first = sender.commit_bedrock_batch(&batch_for(position, 0), 0);
        sender.enqueue_chunk(position);

        assert_eq!(sender.ready_chunks_count(), 0);
        assert!(!sender.is_chunk_ready(&position));

        sender.mark_delivered(&[(position, first[0].delivery_token)]);
        assert_eq!(sender.ready_chunks_count(), 1);
    }

    #[test]
    fn deferred_first_delivery_is_pending_again() {
        let position = Vector2::new(4, 0);
        let mut sender = ChunkSender::new();

        sender.enqueue_chunk(position);
        let dispatch = sender.commit_bedrock_batch(&batch_for(position, 0), 0);
        sender.fail_delivery(&[(
            position,
            dispatch[0].delivery_token,
            DeliveryFailure::Deferred,
        )]);

        assert!(sender.pending_chunks.contains(&position));
        assert!(!sender.is_chunk_sent(&position));
    }

    #[test]
    fn cancelled_delivery_is_parked_then_retried() {
        let position = Vector2::new(-1, 6);
        let mut sender = ChunkSender::new();

        sender.enqueue_chunk(position);
        let dispatch = sender.commit_bedrock_batch(&batch_for(position, 0), 0);
        sender.fail_delivery(&[(
            position,
            dispatch[0].delivery_token,
            DeliveryFailure::Cancelled,
        )]);

        assert!(!sender.pending_chunks.contains(&position));
        assert!(!sender.is_chunk_sent(&position));

        for _ in 1..CANCELLED_RETRY_TICKS {
            sender.batch_tick += 1;
            sender.release_parked();
            assert!(!sender.pending_chunks.contains(&position));
        }
        sender.batch_tick += 1;
        sender.release_parked();
        assert!(sender.pending_chunks.contains(&position));
    }

    #[test]
    fn cancelled_java_chunk_is_parked() {
        let position = Vector2::new(9, -9);
        let batch = batch_for(position, 3);
        let mut sender = ChunkSender::new();
        sender.enqueue_chunk(position);

        sender.cancel_pending(&batch, 3, &[position]);

        assert!(!sender.pending_chunks.contains(&position));
        assert!(sender.parked_chunks.contains_key(&position));
    }

    #[test]
    fn java_chunk_that_never_encodes_is_dropped_after_bounded_attempts() {
        let position = Vector2::new(4, 4);
        let batch = batch_for(position, 0);
        let mut sender = ChunkSender::new();
        sender.enqueue_chunk(position);

        for attempt in 1..=MAX_ENCODE_ATTEMPTS {
            assert!(sender.pending_chunks.contains(&position));
            sender.settle_unencoded(&batch, &[]);
            assert_eq!(
                sender.pending_chunks.contains(&position),
                attempt < MAX_ENCODE_ATTEMPTS
            );
        }
    }

    #[test]
    fn re_enqueue_or_reset_drops_a_parked_chunk() {
        let position = Vector2::new(2, -7);
        let mut sender = ChunkSender::new();
        sender.enqueue_chunk(position);
        sender.cancel_pending(&batch_for(position, 0), 0, &[position]);

        // A republish sends now instead of waiting.
        sender.enqueue_chunk(position);
        assert!(sender.pending_chunks.contains(&position));
        assert!(!sender.parked_chunks.contains_key(&position));

        // A stale queue entry cannot release a later park early.
        sender.batch_tick += 1;
        sender.cancel_pending(&batch_for(position, 0), 0, &[position]);
        sender.batch_tick += CANCELLED_RETRY_TICKS - 1;
        sender.release_parked();
        assert!(!sender.pending_chunks.contains(&position));

        sender.reset();
        assert!(sender.parked_chunks.is_empty());
    }

    #[test]
    fn unencodable_delivery_gives_up_after_bounded_attempts() {
        let position = Vector2::new(7, 7);
        let mut sender = ChunkSender::new();
        sender.enqueue_chunk(position);

        for attempt in 1..=MAX_ENCODE_ATTEMPTS {
            let dispatch = sender.commit_bedrock_batch(&batch_for(position, 0), 0);
            assert_eq!(dispatch.len(), 1);
            sender.fail_delivery(&[(
                position,
                dispatch[0].delivery_token,
                DeliveryFailure::Unencodable,
            )]);
            assert_eq!(
                sender.pending_chunks.contains(&position),
                attempt < MAX_ENCODE_ATTEMPTS
            );
        }

        assert!(!sender.is_chunk_sent(&position));
        assert!(
            sender
                .commit_bedrock_batch(&batch_for(position, 0), 0)
                .is_empty()
        );
    }

    #[test]
    fn failed_resend_of_a_held_chunk_keeps_it_held() {
        let position = Vector2::new(0, -5);
        let mut sender = ChunkSender::new();

        sender.enqueue_chunk(position);
        let first = sender.commit_bedrock_batch(&batch_for(position, 0), 0);
        sender.mark_delivered(&[(position, first[0].delivery_token)]);

        sender.enqueue_chunk(position);
        let resend = sender.commit_bedrock_batch(&batch_for(position, 0), 0);
        let ready = sender.fail_delivery(&[(
            position,
            resend[0].delivery_token,
            DeliveryFailure::Cancelled,
        )]);

        // Entities that appeared while the resend was in flight need pairing now
        assert_eq!(ready, [position]);
        assert!(sender.is_chunk_ready(&position));
        assert!(sender.parked_chunks.contains_key(&position));
    }

    #[test]
    fn failed_redispatch_keeps_chunk_sent_while_an_earlier_one_is_in_flight() {
        let position = Vector2::new(1, 1);
        let mut sender = ChunkSender::new();

        sender.enqueue_chunk(position);
        let first = sender.commit_bedrock_batch(&batch_for(position, 0), 0);
        sender.enqueue_chunk(position);
        let second = sender.commit_bedrock_batch(&batch_for(position, 0), 0);

        sender.fail_delivery(&[(
            position,
            second[0].delivery_token,
            DeliveryFailure::Cancelled,
        )]);
        assert!(sender.is_chunk_sent(&position));

        // The earlier dispatch then queues the chunk
        let delivered = sender.mark_delivered(&[(position, first[0].delivery_token)]);
        assert_eq!(delivered, vec![position]);
        assert!(sender.is_chunk_ready(&position));
    }

    #[test]
    fn earlier_dispatch_delivered_before_a_failed_redispatch_keeps_chunk_held() {
        let position = Vector2::new(1, 2);
        let mut sender = ChunkSender::new();

        sender.enqueue_chunk(position);
        let first = sender.commit_bedrock_batch(&batch_for(position, 0), 0);
        sender.enqueue_chunk(position);
        let second = sender.commit_bedrock_batch(&batch_for(position, 0), 0);

        sender.mark_delivered(&[(position, first[0].delivery_token)]);
        sender.fail_delivery(&[(
            position,
            second[0].delivery_token,
            DeliveryFailure::Deferred,
        )]);

        assert!(sender.is_chunk_ready(&position));
        assert!(sender.pending_chunks.contains(&position));
    }

    #[test]
    fn out_of_band_send_and_re_enqueue_leave_no_parked_entry() {
        let position = Vector2::new(6, 6);
        let mut sender = ChunkSender::new();
        sender.enqueue_chunk(position);
        sender.cancel_pending(&batch_for(position, 0), 0, &[position]);

        sender.mark_sent_out_of_band(position);
        sender.batch_tick += CANCELLED_RETRY_TICKS;
        sender.release_parked();

        assert!(!sender.pending_chunks.contains(&position));
    }

    #[test]
    fn cancelled_redispatch_waits_for_its_retry_tick() {
        let position = Vector2::new(-4, -4);
        let mut sender = ChunkSender::new();

        sender.enqueue_chunk(position);
        let dispatch = sender.commit_bedrock_batch(&batch_for(position, 0), 0);
        sender.enqueue_chunk(position);
        sender.fail_delivery(&[(
            position,
            dispatch[0].delivery_token,
            DeliveryFailure::Cancelled,
        )]);

        assert!(!sender.pending_chunks.contains(&position));
        assert!(sender.parked_chunks.contains_key(&position));
    }

    fn batch_for(position: Vector2<i32>, epoch_snapshot: u32) -> PreparedBatch {
        PreparedBatch {
            chunks: vec![PreparedChunk {
                position,
                chunk: ChunkData::empty_sync(position.x, position.y),
            }],
            epoch_snapshot,
            target_version: JavaMinecraftVersion::V_1_20_2,
        }
    }
}
