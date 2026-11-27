use std::sync::atomic::{
    AtomicBool, AtomicU32,
    Ordering::{self, Relaxed},
};
use std::sync::{Arc, MutexGuard, OnceLock, RwLock, RwLockReadGuard};

use bytes::{BufMut, Bytes};
use crossbeam::atomic::AtomicCell;
use dashmap::{DashMap, mapref::entry::Entry, try_result::TryResult};
use pumpkin_data::data_component_impl::EquipmentSlot;
use pumpkin_data::effect::StatusEffect;
use pumpkin_data::entity::EntityType;
use pumpkin_data::item_stack::ItemStack;
use pumpkin_data::packet::CURRENT_MC_VERSION;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use sculith_protocol::bedrock::client::move_actor_delta::{
    CMoveActorDelta, MOVE_ACTOR_DELTA_FLAG_HAS_HEAD_YAW, MOVE_ACTOR_DELTA_FLAG_HAS_PITCH,
    MOVE_ACTOR_DELTA_FLAG_HAS_X, MOVE_ACTOR_DELTA_FLAG_HAS_Y, MOVE_ACTOR_DELTA_FLAG_HAS_YAW,
    MOVE_ACTOR_DELTA_FLAG_HAS_Z, MOVE_ACTOR_DELTA_FLAG_ON_GROUND,
};
use sculith_protocol::bedrock::client::remove_actor::CRemoveActor;
use sculith_protocol::bedrock::client::set_actor_data::{
    CSetActorData, PropertySyncData, SyncedActorDataList,
};
use sculith_protocol::bedrock::client::take_item_actor::CTakeItemActor;
use sculith_protocol::bedrock::client::{CMovePlayer, CSetActorMotion};
use sculith_protocol::codec::item_stack_seralizer::ItemStackSerializer;
use sculith_protocol::codec::var_int::VarInt;
use sculith_protocol::codec::var_long::VarLong;
use sculith_protocol::codec::var_ulong::VarULong;
use sculith_protocol::java::client::play::{
    CEntityPositionSync, CEntityVelocity, CHeadRot, CRemoveEntities, CSetEntityMetadata,
    CSetEquipment, CSetPassengers, CTakeItemEntity, CUpdateEntityPos, CUpdateEntityPosRot,
    CUpdateEntityRot, Metadata,
};
use sculith_protocol::{BClientPacket, ClientPacket};
use sculith_util::GameMode;
use sculith_util::math::vector2::Vector2;
use sculith_util::math::vector3::Vector3;
use sculith_util::math::{pack_degrees, position::chunk_section_from_pos};
use sculith_world::cylindrical_chunk_iterator::Cylindrical;
use uuid::Uuid;

use crate::entity::attributes::bedrock_health_update;
use crate::entity::effect::bedrock_particles::BedrockEffectParticles;
use crate::entity::living::{RegionalActorDelivery, RegionalEffectData};
use crate::entity::player::Player;
use crate::entity::{Entity, EntityBase, bedrock_actor_spawn_packet, item::ItemEntity};
use crate::net::ClientPlatform;
use crate::net::bedrock::BedrockClient;
use crate::net::chunk_sender::ChunkSender;
use crate::net::java::JavaClient;
use crate::plugin::api::events::entity::entity_regain_health::EntityRegainHealthEvent;
use crate::plugin::server::packet::{BedrockConnectionPacketSentEvent, PacketSentEvent};
use crate::server::Server;
use crate::world::World;
use crate::world::chunker::get_view_distance;

const MIN_MOVE_DISTANCE_SQUARED: f32 = 7.629_394_5e-6;
const MIN_INDEXED_PLAYER_VIEWS: usize = 16;
const BEDROCK_POSITION_FIELDS: u16 =
    MOVE_ACTOR_DELTA_FLAG_HAS_X | MOVE_ACTOR_DELTA_FLAG_HAS_Y | MOVE_ACTOR_DELTA_FLAG_HAS_Z;
const BEDROCK_ROTATION_FIELDS: u16 = MOVE_ACTOR_DELTA_FLAG_HAS_PITCH
    | MOVE_ACTOR_DELTA_FLAG_HAS_YAW
    | MOVE_ACTOR_DELTA_FLAG_HAS_HEAD_YAW;

/// Vanilla `VecDeltaCodec.encode`: Java `Math.round` on the 1/4096
fn encode_pos(value: f64) -> i64 {
    (value * 4096.0 + 0.5).floor() as i64
}

/// Vanilla `ServerEntity.sendChanges` packet choice.
#[derive(Clone, Copy)]
enum MoveSync {
    Pos(Vector3<i16>),
    Rot,
    PosRot(Vector3<i16>),
    Absolute,
}

impl MoveSync {
    const fn sends_position(self) -> bool {
        !matches!(self, Self::Rot)
    }

    const fn sends_rotation(self) -> bool {
        !matches!(self, Self::Pos(_))
    }
}

#[derive(Clone, Copy)]
struct PlayerViewSnapshot<'a> {
    player: &'a Arc<Player>,
    entity_id: i32,
    pos: Vector3<f64>,
    watched: Cylindrical,
    view_distance: i32,
}

struct PlayerViews<'a> {
    players: Vec<PlayerViewSnapshot<'a>>,
    index: OnceLock<Option<PlayerViewIndex>>,
}

impl<'a> PlayerViews<'a> {
    const fn new(players: Vec<PlayerViewSnapshot<'a>>) -> Self {
        Self {
            players,
            index: OnceLock::new(),
        }
    }

    fn index(&self) -> Option<&PlayerViewIndex> {
        self.index
            .get_or_init(|| {
                PlayerViewIndex::new(
                    self.players
                        .iter()
                        .map(|view| (view.player.gameprofile.id, view.pos, view.view_distance)),
                )
            })
            .as_ref()
    }
}

/// Candidate views are indexed by horizontal position in this tick's snapshot.
struct PlayerViewIndex {
    by_x: Vec<(f64, usize)>,
    by_z: Vec<(f64, usize)>,
    by_uuid: FxHashMap<Uuid, usize>,
    max_view_distance: i32,
}

impl PlayerViewIndex {
    fn new(views: impl ExactSizeIterator<Item = (Uuid, Vector3<f64>, i32)>) -> Option<Self> {
        let mut by_x = Vec::with_capacity(views.len());
        let mut by_z = Vec::with_capacity(views.len());
        let mut by_uuid = FxHashMap::default();
        let mut max_view_distance = 0;
        for (index, (id, position, view_distance)) in views.enumerate() {
            if !position.x.is_finite() || !position.z.is_finite() {
                return None;
            }
            by_x.push((position.x, index));
            by_z.push((position.z, index));
            by_uuid.insert(id, index);
            max_view_distance = max_view_distance.max(view_distance);
        }
        by_x.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        by_z.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        Some(Self {
            by_x,
            by_z,
            by_uuid,
            max_view_distance,
        })
    }

    fn nearby(&self, position: Vector3<f64>, tracking_range: u32) -> Option<&[(f64, usize)]> {
        fn window(entries: &[(f64, usize)], center: f64, radius: f64) -> &[(f64, usize)] {
            let start = entries.partition_point(|&(coordinate, _)| coordinate < center - radius);
            let end = entries.partition_point(|&(coordinate, _)| coordinate <= center + radius);
            &entries[start..end]
        }
        if !position.x.is_finite() || !position.z.is_finite() {
            return None;
        }
        // A conservative boundary leaves exact range and view checks to the tracker.
        let view = i32::try_from(tracking_range)
            .ok()?
            .min(self.max_view_distance);
        let radius = f64::from(view) * 16.0 + 1.0;
        let x = window(&self.by_x, position.x, radius);
        let z = window(&self.by_z, position.z, radius);
        let nearby = if x.len() <= z.len() { x } else { z };
        (nearby.len() < self.by_x.len() / 2).then_some(nearby)
    }

    fn ordered_candidates(
        &self,
        nearby: &[(f64, usize)],
        paired: impl Iterator<Item = Uuid>,
    ) -> Vec<usize> {
        let mut candidates: Vec<_> = nearby.iter().map(|&(_, index)| index).collect();
        // Paired viewers outside the search window still need their removal check.
        candidates.extend(paired.filter_map(|id| self.by_uuid.get(&id).copied()));
        candidates.sort_unstable();
        candidates.dedup();
        candidates
    }
}

struct RegionalActorPackets {
    java: Vec<Bytes>,
    bedrock: Vec<Bytes>,
    bedrock_viewer_equipment: Option<Bytes>,
}

#[derive(Default)]
struct Viewers {
    players: FxHashMap<Uuid, Arc<Player>>,
    snapshot: OnceLock<Arc<[Arc<Player>]>>,
}

impl Viewers {
    fn insert(&mut self, id: Uuid, player: Arc<Player>) -> bool {
        if let std::collections::hash_map::Entry::Vacant(slot) = self.players.entry(id) {
            slot.insert(player);
            self.snapshot.take();
            return true;
        }
        false
    }

    fn remove(&mut self, id: &Uuid) -> bool {
        if self.players.remove(id).is_some() {
            self.snapshot.take();
            return true;
        }
        false
    }

    fn snapshot(&self) -> Arc<[Arc<Player>]> {
        self.snapshot
            .get_or_init(|| self.players.values().cloned().collect())
            .clone()
    }
}

fn try_ready_regional_viewer<'a>(
    player: &'a Player,
    world: &World,
    chunk: Vector2<i32>,
) -> Option<MutexGuard<'a, ChunkSender>> {
    let sender = player.chunk_sender.try_lock().ok()?;
    (player.can_simulate_in(world)
        && player
            .watched_section
            .load()
            .is_within_distance(chunk.x, chunk.y)
        && sender.is_chunk_ready(&chunk))
    .then_some(sender)
}

fn in_tracking_range(
    player_pos: Vector3<f64>,
    entity_pos: Vector3<f64>,
    player_distance: i32,
    tracking_range: u32,
) -> bool {
    let dx = player_pos.x - entity_pos.x;
    let dz = player_pos.z - entity_pos.z;
    let range = f64::from((tracking_range as i32).min(player_distance) * 16);
    dx.mul_add(dx, dz * dz) <= range * range
}

/// Spawn data prepared while a restored livestock actor is still private.
pub(super) struct RestoredMobSpawn {
    tracked: Arc<TrackedEntity>,
    java: Vec<Bytes>,
    bedrock: Bytes,
    position: Vector3<f64>,
    rotation: [f32; 4],
    health: f32,
    age: i32,
}

impl RestoredMobSpawn {
    pub(super) fn prepare(actor: &Arc<dyn EntityBase>) -> Option<Self> {
        let living = actor.get_living_entity()?;
        if !actor.has_client_representation()
            || living.is_dead_or_dying()
            || !living.active_effects.try_lock().ok()?.is_empty()
            || living
                .entity_equipment
                .try_lock()
                .ok()?
                .equipment
                .values()
                .any(|stack| !stack.is_empty())
        {
            return None;
        }
        let base = actor.get_entity();
        Some(Self {
            // The tracker initializes the Java pose baseline used by the spawn packet.
            tracked: EntityTracker::new_tracked_entity(actor),
            java: TrackedEntity::encode_restored_java_actor(actor.as_ref(), None)?,
            bedrock: TrackedEntity::encode_restored_bedrock_actor(actor.as_ref(), None)?,
            position: base.pos.load(),
            rotation: base.bedrock_rotation(),
            health: living.health.load(),
            age: base.age.load(Relaxed),
        })
    }

    fn is_fresh(&self, base: &Entity, health: f32) -> bool {
        base.first_tick.load(Relaxed)
            && base.tick_count.load(Relaxed) == 0
            && base.pos.load() == self.position
            && base.bedrock_rotation() == self.rotation
            && base.age.load(Relaxed) == self.age
            && health.to_bits() == self.health.to_bits()
    }
}

#[derive(Default)]
struct RegionalMoveDelivery {
    viewers: usize,
    queued: usize,
}

impl RegionalMoveDelivery {
    fn reconcile(self, needs_sync: &AtomicBool, update: impl FnOnce()) {
        if self.queued == 0 {
            return;
        }
        // A partial send leaves two baselines. Queue a full pose before sending more deltas.
        let complete = self.queued == self.viewers;
        needs_sync.store(!complete, Relaxed);
        if complete {
            update();
        }
    }
}

pub struct TrackedEntity {
    pub entity: Arc<dyn EntityBase>,
    pub entity_id: i32,
    pub tracking_range: u32,
    pub update_interval: u32,
    pub track_deltas: bool,
    seen_by: RwLock<Viewers>,
    pending_spawns: AtomicU32,
    /// Bedrock players in `seen_by`.
    bedrock_watchers: AtomicU32,
    pub last_section_pos: AtomicCell<Vector3<i32>>,
    pos_dirty: AtomicBool,
    tick_count: AtomicU32,
    teleport_delay: AtomicU32,
    was_on_ground: AtomicBool,
    was_riding: AtomicBool,
    java_pose_needs_sync: AtomicBool,
    bedrock_pos: AtomicCell<Vector3<f64>>,
    /// Packed pitch, yaw, head yaw.
    bedrock_rot: AtomicCell<[u8; 3]>,
    bedrock_pose_needs_sync: AtomicBool,
}

impl TrackedEntity {
    #[must_use]
    pub fn new(
        entity: Arc<dyn EntityBase>,
        range: u32,
        update_interval: u32,
        track_deltas: bool,
    ) -> Self {
        let base = entity.get_entity();
        let entity_id = base.entity_id;
        let pos = base.pos.load();
        let on_ground = base.on_ground.load(Relaxed);
        let bedrock_pos = entity.bedrock_pos();
        let bedrock_rot = Self::bedrock_move_rotation(base);
        let rot = [
            pack_degrees(base.pitch.load()),
            pack_degrees(base.yaw.load()),
            pack_degrees(base.head_yaw.load()),
        ];
        // Vanilla `ServerEntity` ctor: sync state starts at the current values.
        base.last_sent_pos.store(pos);
        base.last_sent_velocity.store(base.velocity.load());
        base.last_sent_pitch.store(rot[0], Relaxed);
        base.last_sent_yaw.store(rot[1], Relaxed);
        base.last_sent_head_yaw.store(rot[2], Relaxed);
        let last_section_pos = chunk_section_from_pos(&base.block_pos.load());
        Self {
            entity,
            entity_id,
            tracking_range: range,
            update_interval,
            track_deltas,
            seen_by: RwLock::default(),
            pending_spawns: AtomicU32::new(0),
            bedrock_watchers: AtomicU32::new(0),
            last_section_pos: AtomicCell::new(last_section_pos),
            pos_dirty: AtomicBool::new(false),
            tick_count: AtomicU32::new(0),
            teleport_delay: AtomicU32::new(0),
            was_on_ground: AtomicBool::new(on_ground),
            was_riding: AtomicBool::new(false),
            java_pose_needs_sync: AtomicBool::new(false),
            bedrock_pos: AtomicCell::new(bedrock_pos),
            bedrock_rot: AtomicCell::new(bedrock_rot),
            bedrock_pose_needs_sync: AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn has_bedrock_watchers(&self) -> bool {
        self.bedrock_watchers.load(Relaxed) > 0
    }

    #[must_use]
    pub fn has_viewers(&self) -> bool {
        !self
            .seen_by
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .players
            .is_empty()
    }

    #[must_use]
    pub fn is_seen_by(&self, uuid: &Uuid) -> bool {
        self.seen_by
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .players
            .contains_key(uuid)
    }

    /// Vanilla `ServerEntity.sendChanges`. Non-player entities only.
    pub fn send_changes(&self, world: &World) {
        let entity = self.entity.get_entity();
        let _motion = entity.hold_actor_motion();
        if entity.is_removed() {
            return;
        }
        if entity.entity_type.mob {
            self.send_pending_bedrock_mob_particles(world);
        }
        self.send_bedrock_move(entity, world);
        let tick = self.tick_count.fetch_add(1, Relaxed);
        let needs_sync = entity.velocity_dirty.swap(false, Ordering::SeqCst);
        // Head yaw stays live every tick, only position/rotation sync is throttled.
        if tick.is_multiple_of(self.update_interval.max(1))
            || needs_sync
            || entity.synched_data.is_dirty()
            || self.java_pose_needs_sync.load(Relaxed)
        {
            let yaw = pack_degrees(entity.yaw.load());
            let pitch = pack_degrees(entity.pitch.load());
            let rot_changed = yaw != entity.last_sent_yaw.load(Relaxed)
                || pitch != entity.last_sent_pitch.load(Relaxed);

            if self.entity.is_passenger() {
                if rot_changed {
                    self.send_move(entity, MoveSync::Rot, yaw, pitch, world);
                    entity.last_sent_yaw.store(yaw, Relaxed);
                    entity.last_sent_pitch.store(pitch, Relaxed);
                }
                entity.last_sent_pos.store(entity.pos.load());
                entity.send_dirty_entity_data();
                self.was_riding.store(true, Relaxed);
            } else {
                let pos = entity.pos.load();
                let sync = self.pick_move(entity, pos, tick, rot_changed);
                if needs_sync || self.track_deltas || entity.is_fall_flying() {
                    self.send_motion(entity, world);
                }
                if let Some(sync) = sync {
                    self.send_move(entity, sync, yaw, pitch, world);
                }
                entity.send_dirty_entity_data();
                if let Some(sync) = sync {
                    if sync.sends_position() {
                        entity.last_sent_pos.store(pos);
                    }
                    if sync.sends_rotation() {
                        entity.last_sent_yaw.store(yaw, Relaxed);
                        entity.last_sent_pitch.store(pitch, Relaxed);
                    }
                }
                self.was_riding.store(false, Relaxed);
            }
        }

        // Vanilla `ServerEntity.sendChanges`: after the needsSync block, `syncVelocity`
        // (`hurtMarked`) sends motion to tracking players (and self, for players).
        if entity.sync_velocity.swap(false, Ordering::SeqCst) {
            self.send_motion(entity, world);
        }

        // Java `CHeadRot` is per-tick. Bedrock watchers of this entity get head yaw from
        // `send_bedrock_move` (`MoveActorDelta` HAS_HEAD_YAW), not from this packet.
        let head_yaw = pack_degrees(entity.head_yaw.load());
        if head_yaw != entity.last_sent_head_yaw.load(Relaxed) {
            self.send_to_tracking_players(&CHeadRot::new(VarInt(self.entity_id), head_yaw), world);
            entity.last_sent_head_yaw.store(head_yaw, Relaxed);
        }
    }

    fn pick_move(
        &self,
        entity: &Entity,
        pos: Vector3<f64>,
        tick: u32,
        rot_changed: bool,
    ) -> Option<MoveSync> {
        let teleport_delay = self.teleport_delay.fetch_add(1, Relaxed) + 1;
        let base = entity.last_sent_pos.load();
        let send_pos = (pos - base).length_squared() >= f64::from(MIN_MOVE_DISTANCE_SQUARED)
            || tick.is_multiple_of(60);
        let force_absolute = self.java_pose_needs_sync.swap(false, Relaxed);
        let delta = match (
            i16::try_from(encode_pos(pos.x) - encode_pos(base.x)),
            i16::try_from(encode_pos(pos.y) - encode_pos(base.y)),
            i16::try_from(encode_pos(pos.z) - encode_pos(base.z)),
        ) {
            (Ok(x), Ok(y), Ok(z)) => Some(Vector3::new(x, y, z)),
            _ => None,
        };
        let on_ground = entity.on_ground.load(Relaxed);
        let is_arrow = [
            &EntityType::ARROW,
            &EntityType::SPECTRAL_ARROW,
            &EntityType::TRIDENT,
        ]
        .contains(&entity.entity_type);

        match delta {
            Some(delta)
                if !force_absolute
                    && teleport_delay <= 400
                    && !self.was_riding.load(Relaxed)
                    && self.was_on_ground.load(Relaxed) == on_ground =>
            {
                if (send_pos && rot_changed) || is_arrow {
                    Some(MoveSync::PosRot(delta))
                } else if send_pos {
                    Some(MoveSync::Pos(delta))
                } else if rot_changed {
                    Some(MoveSync::Rot)
                } else {
                    None
                }
            }
            _ => {
                self.was_on_ground.store(on_ground, Relaxed);
                self.teleport_delay.store(0, Relaxed);
                Some(MoveSync::Absolute)
            }
        }
    }

    fn send_motion(&self, entity: &Entity, world: &World) {
        let velocity = entity.velocity.load();
        if entity.velocity_changed_since_sent() {
            entity.last_sent_velocity.store(velocity);
            self.send_to_tracking_players_editioned(
                &CEntityVelocity::new(VarInt(self.entity_id), velocity),
                &CSetActorMotion {
                    target_runtime_id: VarULong(self.entity_id as u64),
                    motion: velocity.to_f32_lossy(),
                    tick: VarULong(0),
                },
                world,
            );
        }
    }

    /// Java only -> Bedrock uses [`Self::send_bedrock_move`].
    fn send_move(&self, entity: &Entity, sync: MoveSync, yaw: u8, pitch: u8, world: &World) {
        let on_ground = entity.on_ground.load(Relaxed);
        let id = VarInt(self.entity_id);
        match sync {
            MoveSync::Pos(delta) => {
                self.send_to_tracking_players(&CUpdateEntityPos::new(id, delta, on_ground), world);
            }
            MoveSync::Rot => {
                self.send_to_tracking_players(
                    &CUpdateEntityRot::new(id, yaw, pitch, on_ground),
                    world,
                );
            }
            MoveSync::PosRot(delta) => self.send_to_tracking_players(
                &CUpdateEntityPosRot::new(id, delta, yaw, pitch, on_ground),
                world,
            ),
            MoveSync::Absolute => self.send_to_tracking_players(&self.absolute_move(entity), world),
        }
    }

    fn absolute_move(&self, entity: &Entity) -> CEntityPositionSync {
        CEntityPositionSync::new(
            VarInt(self.entity_id),
            entity.pos.load(),
            entity.velocity.load(),
            entity.yaw.load(),
            entity.pitch.load(),
            entity.on_ground.load(Relaxed),
        )
    }

    fn bedrock_move_packet(
        &self,
        entity: &Entity,
        pos: Vector3<f64>,
        rot: [u8; 3],
        mut flags: u16,
    ) -> CMoveActorDelta {
        if entity.on_ground.load(Relaxed) {
            flags |= MOVE_ACTOR_DELTA_FLAG_ON_GROUND;
        }
        CMoveActorDelta::new(
            VarULong(self.entity_id as u64),
            flags,
            pos.x as f32,
            pos.y as f32,
            pos.z as f32,
            rot[0],
            rot[1],
            rot[2],
        )
    }

    /// Bedrock -> every tick, only changed fields.
    fn send_bedrock_move(&self, entity: &Entity, world: &World) {
        let pos = self.entity.bedrock_pos();
        let rot = Self::bedrock_move_rotation(entity);
        let last_pos = self.bedrock_pos.load();
        let last_rot = self.bedrock_rot.load();

        let passenger = self.entity.is_passenger();
        let mut flags = if !passenger && self.bedrock_pose_needs_sync.swap(false, Relaxed) {
            BEDROCK_POSITION_FIELDS | BEDROCK_ROTATION_FIELDS
        } else {
            0
        };
        if !passenger {
            for (now, last, flag) in [
                (pos.x, last_pos.x, MOVE_ACTOR_DELTA_FLAG_HAS_X),
                (pos.y, last_pos.y, MOVE_ACTOR_DELTA_FLAG_HAS_Y),
                (pos.z, last_pos.z, MOVE_ACTOR_DELTA_FLAG_HAS_Z),
            ] {
                if (now as f32).to_bits() != (last as f32).to_bits() {
                    flags |= flag;
                }
            }
        }
        for (i, flag) in [
            MOVE_ACTOR_DELTA_FLAG_HAS_PITCH,
            MOVE_ACTOR_DELTA_FLAG_HAS_YAW,
            MOVE_ACTOR_DELTA_FLAG_HAS_HEAD_YAW,
        ]
        .into_iter()
        .enumerate()
        {
            if rot[i] != last_rot[i] {
                flags |= flag;
            }
        }
        if flags == 0 {
            return;
        }

        if flags & BEDROCK_POSITION_FIELDS != 0 {
            self.bedrock_pos.store(pos);
        }
        self.bedrock_rot.store(rot);
        if !self.has_bedrock_watchers() {
            return;
        }
        self.send_to_tracking_players_bedrock(
            &self.bedrock_move_packet(entity, pos, rot, flags),
            world,
        );
    }

    fn bedrock_move_rotation(entity: &Entity) -> [u8; 3] {
        let [pitch, yaw, head_yaw, _] = entity.bedrock_rotation();
        [pitch, yaw, head_yaw].map(pack_degrees)
    }

    fn collect_indirect_passengers(
        entity: &Arc<dyn EntityBase>,
        result: &mut Vec<Arc<dyn EntityBase>>,
    ) {
        if let Ok(passengers) = entity.get_entity().passengers.try_lock() {
            for passenger in passengers.iter() {
                result.push(passenger.clone());
                Self::collect_indirect_passengers(passenger, result);
            }
        }
    }

    #[must_use]
    pub fn get_effective_range(&self) -> u32 {
        let mut effective_range = self.tracking_range;
        let mut passengers = Vec::new();
        Self::collect_indirect_passengers(&self.entity, &mut passengers);
        for passenger in passengers {
            let passenger_range = passenger.get_entity().entity_type.client_tracking_range;
            if passenger_range > effective_range {
                effective_range = passenger_range;
            }
        }
        effective_range
    }

    fn broadcast_to_player(&self, player: &Player) -> bool {
        self.entity.get_player().is_none_or(|target_player| {
            player.gamemode.load() == GameMode::Spectator
                || target_player.gamemode.load() != GameMode::Spectator
        })
    }

    pub fn update_player(&self, player: &Arc<Player>, _world: &World) {
        if player.get_entity().entity_id == self.entity_id {
            return;
        }
        let entity_chunk = self.entity.get_entity().chunk_pos.load();
        if !player
            .watched_section
            .load()
            .is_within_distance(entity_chunk.x, entity_chunk.y)
        {
            self.remove_player(player);
            return;
        }
        let player_pos = player.get_entity().pos.load();
        let player_vd = get_view_distance(player).get() as i32;
        let entity_pos = self.entity.get_entity().pos.load();
        self.update_player_in_view(
            player,
            player_pos,
            player_vd,
            entity_chunk,
            entity_pos,
            self.get_effective_range(),
            true,
        );
    }

    #[expect(clippy::too_many_arguments)]
    fn update_player_in_view(
        &self,
        player: &Arc<Player>,
        player_pos: Vector3<f64>,
        player_vd: i32,
        entity_chunk: Vector2<i32>,
        entity_pos: Vector3<f64>,
        effective_range: u32,
        chunk_changed: bool,
    ) {
        if !player.initial_spawn_complete.load(Ordering::Acquire)
            || player.client.closed()
            || self.entity.get_entity().is_removed()
            || !self.entity.has_client_representation()
        {
            self.remove_player(player);
            return;
        }
        if in_tracking_range(player_pos, entity_pos, player_vd, effective_range)
            && self.broadcast_to_player(player)
        {
            if !chunk_changed
                && self
                    .seen_by
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .players
                    .contains_key(&player.gameprofile.id)
            {
                return;
            }
            // Vanilla `isChunkTracked`: never spawn before the chunk packet.
            if player
                .chunk_sender
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_chunk_ready(&entity_chunk)
            {
                if chunk_changed
                    && self
                        .seen_by
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .players
                        .contains_key(&player.gameprofile.id)
                {
                    return;
                }
                // A drop stays at the checked chunk while its spawn is queued.
                let base = self.entity.get_entity();
                let pairing = base.hold_actor_motion();
                if base.is_removed() {
                    self.remove_player(player);
                    return;
                }
                if pairing.is_some() && base.chunk_pos.load() != entity_chunk {
                    return;
                }
                let inserted = {
                    let mut viewers = self
                        .seen_by
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let inserted = viewers.insert(player.gameprofile.id, player.clone());
                    if inserted {
                        self.pending_spawns.fetch_add(1, Relaxed);
                        if matches!(player.client.as_ref(), ClientPlatform::Bedrock(_)) {
                            self.bedrock_watchers.fetch_add(1, Relaxed);
                        }
                    }
                    inserted
                };
                if inserted {
                    self.add_pairing(player);
                    self.pending_spawns.fetch_sub(1, Ordering::Release);
                }
                return;
            }
        }
        self.remove_player(player);
    }

    pub fn update_players(&self, players: &[Arc<Player>], _world: &World) {
        if players.is_empty() {
            return;
        }
        let entity_chunk = self.entity.get_entity().chunk_pos.load();
        let mut lazy_state: Option<(Vector3<f64>, u32)> = None;
        for player in players {
            let player_entity = player.get_entity();
            if player_entity.entity_id == self.entity_id {
                continue;
            }
            if !player
                .watched_section
                .load()
                .is_within_distance(entity_chunk.x, entity_chunk.y)
            {
                self.remove_player(player);
                continue;
            }
            let (entity_pos, effective_range) = *lazy_state.get_or_insert_with(|| {
                (
                    self.entity.get_entity().pos.load(),
                    self.get_effective_range(),
                )
            });
            let player_pos = player_entity.pos.load();
            let player_vd = get_view_distance(player).get() as i32;
            self.update_player_in_view(
                player,
                player_pos,
                player_vd,
                entity_chunk,
                entity_pos,
                effective_range,
                true,
            );
        }
    }

    fn update_players_from_snapshots(&self, snapshots: &PlayerViews<'_>, chunk_changed: bool) {
        if snapshots.players.is_empty() {
            return;
        }
        let base = self.entity.get_entity();
        let entity_chunk = base.chunk_pos.load();
        let mut lazy_state: Option<(Vector3<f64>, u32)> = None;
        let candidates = if snapshots.players.len() >= MIN_INDEXED_PLAYER_VIEWS
            && let Some(index) = snapshots.index()
        {
            let position = base.pos.load();
            let range = self.get_effective_range();
            lazy_state = Some((position, range));
            index.nearby(position, range).map(|nearby| {
                index.ordered_candidates(
                    nearby,
                    self.viewers().iter().map(|viewer| viewer.gameprofile.id),
                )
            })
        } else {
            None
        };
        let mut check = |snap: &PlayerViewSnapshot<'_>| {
            if snap.entity_id == self.entity_id {
                return;
            }
            if !snap
                .watched
                .is_within_distance(entity_chunk.x, entity_chunk.y)
            {
                self.remove_player(snap.player);
                return;
            }
            let (entity_pos, effective_range) = *lazy_state.get_or_insert_with(|| {
                (
                    self.entity.get_entity().pos.load(),
                    self.get_effective_range(),
                )
            });
            self.update_player_in_view(
                snap.player,
                snap.pos,
                snap.view_distance,
                entity_chunk,
                entity_pos,
                effective_range,
                chunk_changed,
            );
        };
        if let Some(candidates) = candidates {
            tracing::trace!(
                target: "sculith::entity_tracking",
                entity = self.entity_id,
                roster = snapshots.players.len(),
                candidates = candidates.len(),
                "Narrowed entity visibility candidates"
            );
            for index in candidates {
                check(&snapshots.players[index]);
            }
        } else {
            for snap in &snapshots.players {
                check(snap);
            }
        }
    }

    fn encode_restored_java_actor(
        actor: &dyn EntityBase,
        item: Option<&ItemEntity>,
    ) -> Option<Vec<Bytes>> {
        let base = actor.get_entity();
        let mut packets = vec![
            JavaClient::serialize_packet_for_version(
                &base.create_spawn_packet(),
                CURRENT_MC_VERSION,
            )
            .ok()?,
        ];
        let metadata = if let Some(item) = item {
            Some(item.java_item_spawn_metadata(item.try_spawn_stack()?)?)
        } else {
            actor
                .java_spawn_metadata(CURRENT_MC_VERSION)
                .map(|data| CSetEntityMetadata::new(base.entity_id.into(), data))
        };
        if let Some(packet) = metadata {
            packets
                .push(JavaClient::serialize_packet_for_version(&packet, CURRENT_MC_VERSION).ok()?);
        }
        if actor.get_living_entity().is_some() {
            packets.push(
                JavaClient::serialize_packet_for_version(
                    &Self::initial_head_rotation(base),
                    CURRENT_MC_VERSION,
                )
                .ok()?,
            );
        }
        if let Some(packet) = Self::non_default_java_metadata(base) {
            packets
                .push(JavaClient::serialize_packet_for_version(&packet, CURRENT_MC_VERSION).ok()?);
        }
        Some(packets)
    }

    fn encode_restored_bedrock_actor(
        actor: &dyn EntityBase,
        item: Option<&ItemEntity>,
    ) -> Option<Bytes> {
        let mut data = Vec::new();
        if let Some(item) = item {
            BedrockClient::write_raw_packet(
                &item.bedrock_item_spawn_packet(&item.try_spawn_stack()?),
                &mut data,
            )
            .ok()?;
        } else {
            BedrockClient::write_raw_packet(&bedrock_actor_spawn_packet(actor, true), &mut data)
                .ok()?;
        }
        Some(data.into())
    }

    fn try_pair_restored_actor(
        &self,
        world: &World,
        chunk: Vector2<i32>,
        mob: Option<&RestoredMobSpawn>,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
    ) {
        let base = self.entity.get_entity();
        let position = base.pos.load();
        let item = (self.entity.as_ref() as &dyn std::any::Any).downcast_ref::<ItemEntity>();
        let mut java = None;
        let mut bedrock = None;
        let players = world.players.load();
        for player in players.iter() {
            if player.waiting_for_vehicle(base.entity_uuid)
                || !in_tracking_range(
                    player.get_entity().pos.load(),
                    position,
                    get_view_distance(player).get() as i32,
                    self.tracking_range,
                )
            {
                continue;
            }
            // Keep the terrain ready and the viewer unpaired until the initial packets
            // are queued. Ordinary pairing will pick up anyone we could not serve.
            let Some(_chunk) = try_ready_regional_viewer(player, world, chunk) else {
                continue;
            };
            let Ok(mut viewers) = self.seen_by.try_write() else {
                continue;
            };
            if base.is_removed() || base.chunk_pos.load() != chunk {
                break;
            }
            if let Some(mob) = mob
                && !self
                    .entity
                    .get_living_entity()
                    .is_some_and(|living| mob.is_fresh(base, living.health.load()))
            {
                break;
            }
            if viewers.players.contains_key(&player.gameprofile.id) {
                continue;
            }
            let queued = match player.client.as_ref() {
                ClientPlatform::Java(client) if client.version.load() == CURRENT_MC_VERSION => {
                    let packets = mob.map_or_else(
                        || {
                            java.get_or_insert_with(|| {
                                Self::encode_restored_java_actor(self.entity.as_ref(), item)
                            })
                            .as_ref()
                        },
                        |mob| Some(&mob.java),
                    );
                    let queued = packets
                        .is_some_and(|packets| client.try_enqueue_current_packet_batch(packets));
                    if queued {
                        java_recipients.insert(client.id, player.client.clone());
                    }
                    queued
                }
                ClientPlatform::Bedrock(client) if client.is_current_protocol() => {
                    let data = mob.map_or_else(
                        || {
                            bedrock
                                .get_or_insert_with(|| {
                                    Self::encode_restored_bedrock_actor(self.entity.as_ref(), item)
                                })
                                .as_ref()
                        },
                        |mob| Some(&mob.bedrock),
                    );
                    let queued = data.is_some_and(|data| {
                        client.try_enqueue_packet_data_unobserved(data.clone())
                    });
                    if queued {
                        self.bedrock_watchers.fetch_add(1, Relaxed);
                    }
                    queued
                }
                _ => false,
            };
            if queued {
                viewers.insert(player.gameprofile.id, player.clone());
            }
        }
    }

    fn initial_head_rotation(base: &Entity) -> CHeadRot {
        CHeadRot::new(
            base.entity_id.into(),
            (base.head_yaw.load() * 256.0 / 360.0).rem_euclid(256.0) as u8,
        )
    }

    fn non_default_java_metadata(base: &Entity) -> Option<CSetEntityMetadata> {
        base.synched_data
            .get_non_default_values_for_version(&CURRENT_MC_VERSION)
            .map(|data| CSetEntityMetadata::new(base.entity_id.into(), data))
    }

    #[allow(clippy::too_many_lines)]
    pub fn add_pairing(&self, player: &Arc<Player>) {
        player.client.try_enqueue_spawn_packet(&self.entity);
        player.try_restore_vehicle(&self.entity);

        if let Some(target_player) = self.entity.get_player() {
            let skin_parts = target_player.config.load().skin_parts;
            let target_entity = target_player.get_entity();
            let target_id = target_entity.entity_id;

            if let ClientPlatform::Java(client) = player.client.as_ref() {
                let version = CURRENT_MC_VERSION;
                let mut buf = Vec::new();
                for meta in [
                    Metadata::new(
                        pumpkin_data::tracked_data::player::PLAYER_MODE_CUSTOMISATION,
                        skin_parts,
                    ),
                    Metadata::new(
                        pumpkin_data::tracked_data::player::PLAYER_MODE_CUSTOMIZATION_ID,
                        skin_parts,
                    ),
                ] {
                    let _ = meta.write(&mut buf, &version);
                }
                buf.put_u8(255);
                let meta_packet = CSetEntityMetadata::new(target_id.into(), buf.into());
                if let Ok(packet_data) =
                    JavaClient::serialize_packet_for_version(&meta_packet, version)
                {
                    client.try_enqueue_packet(packet_data);
                }

                let head_yaw = target_entity.head_yaw.load();
                // TODO: use `sculith_util::math::pack_degrees`.
                let head_rot_packet = CHeadRot::new(
                    target_id.into(),
                    (head_yaw * 256.0 / 360.0).rem_euclid(256.0) as u8,
                );
                if let Ok(data) = client.serialize_packet(&head_rot_packet) {
                    client.try_enqueue_packet(data);
                }
            }
        } else if self.entity.get_living_entity().is_some()
            && let ClientPlatform::Java(client) = player.client.as_ref()
        {
            let head_rot_packet = Self::initial_head_rotation(self.entity.get_entity());
            if let Ok(data) = client.serialize_packet(&head_rot_packet) {
                client.try_enqueue_packet(data);
            }
        }

        let vel = self.entity.get_entity().velocity.load();
        if vel.length_squared() > 1e-4 {
            match player.client.as_ref() {
                ClientPlatform::Java(client) => {
                    let motion = CEntityVelocity::new(self.entity_id.into(), vel);
                    if let Ok(data) = client.serialize_packet(&motion) {
                        client.try_enqueue_packet(data);
                    }
                }
                ClientPlatform::Bedrock(client) => {
                    let motion =
                        sculith_protocol::bedrock::client::set_actor_motion::CSetActorMotion {
                            target_runtime_id: (self.entity_id as u64).into(),
                            motion: vel.to_f32_lossy(),
                            tick: 0u64.into(),
                        };
                    if let Ok(data) = client.serialize_packet(&motion) {
                        client.try_enqueue_packet(data);
                    }
                }
            }
        }

        if let ClientPlatform::Java(client) = player.client.as_ref()
            && let Some(packet) = Self::non_default_java_metadata(self.entity.get_entity())
            && let Ok(data) = JavaClient::serialize_packet_for_version(&packet, CURRENT_MC_VERSION)
        {
            client.try_enqueue_packet(data);
        }

        if let Some(living) = self.entity.get_living_entity() {
            if let Ok(equipment_guard) = living.entity_equipment.try_lock() {
                let mut equipment_list = Vec::new();
                for (slot, item_stack) in &equipment_guard.equipment {
                    if !item_stack.is_empty() {
                        equipment_list.push((slot.discriminant(), item_stack.clone()));
                    }
                }
                if !equipment_list.is_empty() {
                    match player.client.as_ref() {
                        ClientPlatform::Java(client) => {
                            let equipment: Vec<(i8, ItemStackSerializer)> = equipment_list
                                .iter()
                                .map(|(slot, stack)| {
                                    (*slot, ItemStackSerializer::from(stack.clone()))
                                })
                                .collect();
                            let packet = CSetEquipment::new(self.entity_id.into(), equipment);
                            if let Ok(data) = client.serialize_packet(&packet) {
                                client.try_enqueue_packet(data);
                            }
                        }
                        ClientPlatform::Bedrock(client) => {
                            use sculith_protocol::bedrock::{
                                client::{
                                    mob_armor_equipment::CMobArmorEquipment,
                                    mob_equipment::CMobEquipment,
                                },
                                network_item::NetworkItemStackDescriptor,
                            };

                            if let Some(main_hand) =
                                equipment_guard.equipment.get(&EquipmentSlot::MAIN_HAND)
                                && !main_hand.is_empty()
                            {
                                let pkt = CMobEquipment {
                                    target_runtime_id: (self.entity_id as u64).into(),
                                    item: main_hand.into(),
                                    slot: 0,
                                    selected_slot: 0,
                                    container_id: 0,
                                };
                                if let Ok(data) = client.serialize_packet(&pkt) {
                                    client.try_enqueue_packet(data);
                                }
                            }
                            if let Some(off_hand) =
                                equipment_guard.equipment.get(&EquipmentSlot::OFF_HAND)
                                && !off_hand.is_empty()
                            {
                                let pkt = CMobEquipment {
                                    target_runtime_id: (self.entity_id as u64).into(),
                                    item: off_hand.into(),
                                    slot: 0,
                                    selected_slot: 0,
                                    container_id: 119,
                                };
                                if let Ok(data) = client.serialize_packet(&pkt) {
                                    client.try_enqueue_packet(data);
                                }
                            }
                            let get_desc = |slot: &EquipmentSlot| {
                                equipment_guard.equipment.get(slot).map_or_else(
                                    NetworkItemStackDescriptor::default,
                                    NetworkItemStackDescriptor::from,
                                )
                            };
                            let has_armor = [
                                &EquipmentSlot::HEAD,
                                &EquipmentSlot::CHEST,
                                &EquipmentSlot::LEGS,
                                &EquipmentSlot::FEET,
                                &EquipmentSlot::BODY,
                            ]
                            .into_iter()
                            .any(|s| {
                                equipment_guard
                                    .equipment
                                    .get(s)
                                    .is_some_and(|stack| !stack.is_empty())
                            });
                            if has_armor {
                                let armor = CMobArmorEquipment {
                                    target_runtime_id: (self.entity_id as u64).into(),
                                    head: get_desc(&EquipmentSlot::HEAD),
                                    chest: get_desc(&EquipmentSlot::CHEST),
                                    legs: get_desc(&EquipmentSlot::LEGS),
                                    feet: get_desc(&EquipmentSlot::FEET),
                                    body: get_desc(&EquipmentSlot::BODY),
                                };
                                if let Ok(data) = client.serialize_packet(&armor) {
                                    client.try_enqueue_packet(data);
                                }
                            }
                        }
                    }
                }
            }

            if let Ok(effects_guard) = living.active_effects.try_lock() {
                for effect in effects_guard.values() {
                    match player.client.as_ref() {
                        ClientPlatform::Java(client) => {
                            let mut flags: i8 = 0;
                            if effect.ambient {
                                flags |= 0x01;
                            }
                            if effect.show_particles {
                                flags |= 0x02;
                            }
                            if effect.show_icon {
                                flags |= 0x04;
                            }
                            let pkt = sculith_protocol::java::client::play::CUpdateMobEffect::new(
                                self.entity_id.into(),
                                i32::from(effect.effect_type.id).into(),
                                i32::from(effect.amplifier).into(),
                                effect.duration.into(),
                                flags,
                            );
                            if let Ok(data) = client.serialize_packet(&pkt) {
                                client.try_enqueue_packet(data);
                            }
                        }
                        ClientPlatform::Bedrock(client) => {
                            let native_id = living
                                .entity
                                .entity_type
                                .mob
                                .then(|| {
                                    BedrockEffectParticles::viewer_effect_id(effect.effect_type)
                                })
                                .flatten();
                            if living.entity.entity_type.mob
                                && client.is_current_protocol()
                                && native_id.is_none()
                            {
                                continue;
                            }
                            let pkt = sculith_protocol::bedrock::client::CMobEffect {
                                target_runtime_id: (self.entity_id as u64).into(),
                                event_id: sculith_protocol::bedrock::client::CMobEffect::EVENT_ADD,
                                effect_id: native_id
                                    .unwrap_or_else(|| effect.effect_type.to_bedrock_id())
                                    .into(),
                                effect_amplifier: i32::from(effect.amplifier).into(),
                                show_particles: effect.show_particles,
                                effect_duration_ticks: effect.duration.into(),
                                tick: 0u64.into(),
                                ambient: effect.ambient,
                            };
                            if let Ok(data) = client.serialize_packet(&pkt) {
                                client.try_enqueue_packet(data);
                            }
                        }
                    }
                }
            }
        }

        if let Ok(passengers) = self.entity.get_entity().passengers.try_lock()
            && !passengers.is_empty()
        {
            match player.client.as_ref() {
                ClientPlatform::Java(client) => {
                    let passenger_ids: Vec<VarInt> = passengers
                        .iter()
                        .map(|p| VarInt(p.get_entity().entity_id))
                        .collect();
                    let packet = CSetPassengers::new(VarInt(self.entity_id), &passenger_ids);
                    if let Ok(data) = client.serialize_packet(&packet) {
                        client.try_enqueue_packet(data);
                    }
                }
                ClientPlatform::Bedrock(client) => {
                    for (idx, p) in passengers.iter().enumerate() {
                        let link = sculith_protocol::bedrock::client::CSetActorLink {
                            link: sculith_protocol::bedrock::client::common::ActorLink {
                                ridden_unique_id: i64::from(self.entity_id).into(),
                                rider_unique_id: i64::from(p.get_entity().entity_id).into(),
                                link_type: if idx == 0 { 1 } else { 2 },
                                immediate: true,
                                rider_initiated: false,
                                vehicle_angular_velocity: 0.0,
                            },
                        };
                        if let Ok(data) = client.serialize_packet(&link) {
                            client.try_enqueue_packet(data);
                        }
                    }
                }
            }
        }

        if let Ok(vehicle_guard) = self.entity.get_entity().vehicle.try_lock()
            && let Some(vehicle) = vehicle_guard.as_ref().and_then(std::sync::Weak::upgrade)
            && let Ok(vehicle_passengers) = vehicle.get_entity().passengers.try_lock()
        {
            match player.client.as_ref() {
                ClientPlatform::Java(client) => {
                    let passenger_ids: Vec<VarInt> = vehicle_passengers
                        .iter()
                        .map(|p| VarInt(p.get_entity().entity_id))
                        .collect();
                    let packet =
                        CSetPassengers::new(VarInt(vehicle.get_entity().entity_id), &passenger_ids);
                    if let Ok(data) = client.serialize_packet(&packet) {
                        client.try_enqueue_packet(data);
                    }
                }
                ClientPlatform::Bedrock(client) => {
                    let vehicle_id = vehicle.get_entity().entity_id;
                    for (idx, p) in vehicle_passengers.iter().enumerate() {
                        if p.get_entity().entity_id == self.entity_id {
                            let link = sculith_protocol::bedrock::client::CSetActorLink {
                                link: sculith_protocol::bedrock::client::common::ActorLink {
                                    ridden_unique_id: i64::from(vehicle_id).into(),
                                    rider_unique_id: i64::from(self.entity_id).into(),
                                    link_type: if idx == 0 { 1 } else { 2 },
                                    immediate: true,
                                    rider_initiated: false,
                                    vehicle_angular_velocity: 0.0,
                                },
                            };
                            if let Ok(data) = client.serialize_packet(&link) {
                                client.try_enqueue_packet(data);
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn remove_pairing(&self, player: &Player) {
        let entity_ids = [self.entity_id.into()];
        match player.client.as_ref() {
            ClientPlatform::Java(client) => {
                let packet = CRemoveEntities::new(&entity_ids);
                if let Ok(data) = client.serialize_packet(&packet) {
                    client.try_enqueue_packet(data);
                }
            }
            ClientPlatform::Bedrock(client) => {
                let packet = CRemoveActor::new(VarLong(i64::from(self.entity_id)));
                if let Ok(data) = client.serialize_packet(&packet) {
                    client.try_enqueue_packet(data);
                }
            }
        }
    }

    pub fn broadcast_removed(&self, _world: &World) {
        let (seen_by, had_bedrock) = {
            let mut guard = self
                .seen_by
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let had_bedrock = self.bedrock_watchers.swap(0, Relaxed) > 0;
            (std::mem::take(&mut *guard).players, had_bedrock)
        };
        if seen_by.is_empty() {
            return;
        }
        let entity_ids = [self.entity_id.into()];
        let je_packet = CRemoveEntities::new(&entity_ids);
        World::broadcast_java_players(&je_packet, seen_by.values());
        if had_bedrock {
            let be_packet = CRemoveActor::new(VarLong(i64::from(self.entity_id)));
            World::broadcast_bedrock_grouped(
                &be_packet,
                seen_by.values().filter_map(|p| match p.client.as_ref() {
                    ClientPlatform::Bedrock(be_client) => Some(be_client),
                    ClientPlatform::Java(_) => None,
                }),
            );
        }
    }

    fn try_regional_move_viewers(
        &self,
        chunk: Vector2<i32>,
    ) -> Option<RwLockReadGuard<'_, Viewers>> {
        let section = self.last_section_pos.load();
        if (section.x, section.z) != (chunk.x, chunk.y) {
            return None;
        }
        self.try_regional_pairings()
    }

    fn try_regional_pairings(&self) -> Option<RwLockReadGuard<'_, Viewers>> {
        // Keep each existing pairing alive until its frame is queued. Spawns run
        // outside this lock because their callbacks can themselves change viewers.
        let viewers = self.seen_by.try_read().ok()?;
        (self.pending_spawns.load(Ordering::Acquire) == 0).then_some(viewers)
    }

    /// The region holds the actor motion guard and its current chunk assignment.
    fn try_publish_actor_move(
        &self,
        world: &World,
        entity: &Entity,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
    ) {
        let chunk = entity.chunk_pos.load();
        let section = self.last_section_pos.load();
        let crossed_chunk = (section.x, section.z) != (chunk.x, chunk.y);
        if crossed_chunk
            && !entity.entity_type.mob
            && !crate::entity::projectile::regional::supports_regional_flight(entity.entity_type)
        {
            return;
        }
        // Full positions can reach already paired viewers before the world
        // refreshes the tracking section. The new chunk still has to be ready.
        let Some(viewers) = self.try_regional_pairings() else {
            return;
        };
        let pos = entity.pos.load();
        // A living actor's world tick count can stay fixed during regional travel.
        let send_java = (entity.entity_type.mob
            || entity
                .tick_count
                .load(Relaxed)
                .cast_unsigned()
                .is_multiple_of(self.update_interval.max(1)))
            && (self.java_pose_needs_sync.load(Relaxed)
                || (pos - entity.last_sent_pos.load()).length_squared()
                    >= f64::from(MIN_MOVE_DISTANCE_SQUARED));
        let bedrock_pos = self.entity.bedrock_pos();
        let send_bedrock = self.has_bedrock_watchers()
            && (self.bedrock_pose_needs_sync.load(Relaxed)
                || bedrock_pos.to_f32_lossy() != self.bedrock_pos.load().to_f32_lossy());
        if !send_java && !send_bedrock {
            return;
        }

        let bedrock_rot = Self::bedrock_move_rotation(entity);
        let mut java_packet = None;
        let mut bedrock_packet = None;
        let mut java = RegionalMoveDelivery::default();
        let mut bedrock = RegionalMoveDelivery::default();
        for player in viewers.players.values() {
            let in_range = !crossed_chunk
                || in_tracking_range(
                    player.get_entity().pos.load(),
                    pos,
                    get_view_distance(player).get() as i32,
                    self.tracking_range,
                );
            match player.client.as_ref() {
                ClientPlatform::Java(client) if send_java => {
                    java.viewers += 1;
                    if !in_range || client.version.load() != CURRENT_MC_VERSION {
                        continue;
                    }
                    let Some(_chunk) = try_ready_regional_viewer(player, world, chunk) else {
                        continue;
                    };
                    let data = java_packet.get_or_insert_with(|| {
                        JavaClient::serialize_packet_for_version(
                            &self.absolute_move(entity),
                            CURRENT_MC_VERSION,
                        )
                    });
                    if data
                        .as_ref()
                        .is_ok_and(|data| client.try_enqueue_packet_data_unobserved(data.clone()))
                    {
                        java.queued += 1;
                        java_recipients
                            .entry(client.id)
                            .or_insert_with(|| player.client.clone());
                    }
                }
                ClientPlatform::Bedrock(client) if send_bedrock => {
                    bedrock.viewers += 1;
                    if !in_range || !client.is_current_protocol() {
                        continue;
                    }
                    let Some(_chunk) = try_ready_regional_viewer(player, world, chunk) else {
                        continue;
                    };
                    // Each included coordinate carries the full position on that axis.
                    let data = bedrock_packet.get_or_insert_with(|| {
                        client.serialize_packet(&self.bedrock_move_packet(
                            entity,
                            bedrock_pos,
                            bedrock_rot,
                            BEDROCK_POSITION_FIELDS | BEDROCK_ROTATION_FIELDS,
                        ))
                    });
                    bedrock.queued +=
                        usize::from(data.as_ref().is_ok_and(|data| {
                            client.try_enqueue_packet_data_unobserved(data.clone())
                        }));
                }
                _ => {}
            }
        }

        java.reconcile(&self.java_pose_needs_sync, || {
            self.update_regional_java_baseline(entity, pos);
        });
        bedrock.reconcile(&self.bedrock_pose_needs_sync, || {
            self.bedrock_pos.store(bedrock_pos);
            self.bedrock_rot.store(bedrock_rot);
        });
    }

    fn update_regional_java_baseline(&self, entity: &Entity, pos: Vector3<f64>) {
        entity.last_sent_pos.store(pos);
        entity
            .last_sent_yaw
            .store(pack_degrees(entity.yaw.load()), Relaxed);
        entity
            .last_sent_pitch
            .store(pack_degrees(entity.pitch.load()), Relaxed);
        self.was_on_ground
            .store(entity.on_ground.load(Relaxed), Relaxed);
        self.was_riding.store(false, Relaxed);
        self.teleport_delay.store(0, Relaxed);
    }

    fn try_publish_drop_removal(
        &self,
        world: &World,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
    ) {
        let Ok(mut viewers) = self.seen_by.try_write() else {
            return;
        };
        let mut java = None;
        let mut bedrock = None;
        let ids = [self.entity_id.into()];
        let mut sent = 0;
        // Successful recipients leave this set. The world drains remaining
        // viewers through the usual translated/observed broadcast.
        viewers.players.retain(|_, player| {
            if !player.can_simulate_in(world) {
                return true;
            }
            let queued = match player.client.as_ref() {
                ClientPlatform::Java(client) if client.version.load() == CURRENT_MC_VERSION => {
                    let data = java.get_or_insert_with(|| {
                        JavaClient::serialize_packet_for_version(
                            &CRemoveEntities::new(&ids),
                            CURRENT_MC_VERSION,
                        )
                    });
                    let queued = data
                        .as_ref()
                        .is_ok_and(|data| client.try_enqueue_packet_data_unobserved(data.clone()));
                    if queued {
                        java_recipients
                            .entry(client.id)
                            .or_insert_with(|| player.client.clone());
                    }
                    queued
                }
                ClientPlatform::Bedrock(client) if client.is_current_protocol() => {
                    let data = bedrock.get_or_insert_with(|| {
                        client.serialize_packet(&CRemoveActor::new(VarLong(i64::from(
                            self.entity_id,
                        ))))
                    });
                    let queued = data
                        .as_ref()
                        .is_ok_and(|data| client.try_enqueue_packet_data_unobserved(data.clone()));
                    if queued {
                        self.bedrock_watchers.fetch_sub(1, Relaxed);
                    }
                    queued
                }
                _ => false,
            };
            sent += usize::from(queued);
            !queued
        });
        if sent != 0 {
            viewers.snapshot.take();
            tracing::trace!(
                entity = self.entity_id,
                actor_type = self.entity.get_entity().entity_type.resource_name,
                sent,
                "Published regional drop removal"
            );
        }
    }

    /// Vanilla `TrackedEntity.removePlayer`: despawn on the client, only if it was paired.
    pub fn remove_player(&self, player: &Player) {
        if self.remove_viewer(player) {
            self.remove_pairing(player);
        }
    }

    /// Forgets the player as a viewer without a removal packet; the client already dropped it.
    pub fn forget_player(&self, player: &Player) {
        self.remove_viewer(player);
    }

    fn remove_viewer(&self, player: &Player) -> bool {
        if !self
            .seen_by
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .players
            .contains_key(&player.gameprofile.id)
        {
            return false;
        }
        let mut viewers = self
            .seen_by
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let removed = viewers.remove(&player.gameprofile.id);
        if removed && matches!(player.client.as_ref(), ClientPlatform::Bedrock(_)) {
            self.bedrock_watchers.fetch_sub(1, Relaxed);
        }
        removed
    }

    fn viewers(&self) -> Arc<[Arc<Player>]> {
        self.seen_by
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot()
    }

    fn send_pending_bedrock_mob_particles(&self, world: &World) {
        let Some(living) = self.entity.get_living_entity() else {
            return;
        };
        living.publish_dirty_bedrock_effect_particles(|packed| {
            if self.has_bedrock_watchers() {
                let packet = BedrockEffectParticles::packet(self.entity_id, packed);
                let viewers = self.viewers();
                World::broadcast_bedrock_grouped(
                    &packet,
                    viewers
                        .iter()
                        .filter(|viewer| viewer.can_simulate_in(world))
                        .filter_map(|viewer| viewer.client.bedrock())
                        .filter(|client| client.is_current_protocol()),
                );
            }
        });
    }

    pub fn send_to_tracking_players<P: ClientPacket + Sync>(&self, packet: &P, _world: &World) {
        let seen_by = self.viewers();
        if seen_by.is_empty() {
            return;
        }
        World::broadcast_java_players(packet, seen_by.iter());
    }

    pub fn send_to_tracking_players_bedrock<P: BClientPacket + Sync>(
        &self,
        packet: &P,
        _world: &World,
    ) {
        if !self.has_bedrock_watchers() {
            return;
        }
        let seen_by = self.viewers();
        let recipients = seen_by.iter().filter_map(|p| {
            if let ClientPlatform::Bedrock(client) = p.client.as_ref() {
                return Some(client);
            }
            None
        });
        World::broadcast_bedrock_grouped(packet, recipients);
    }

    pub fn send_to_tracking_players_editioned<J: ClientPacket + Sync, B: BClientPacket + Sync>(
        &self,
        je_packet: &J,
        be_packet: &B,
        _world: &World,
    ) {
        let seen_by = self.viewers();
        if seen_by.is_empty() {
            return;
        }
        World::broadcast_java_players(je_packet, seen_by.iter());
        if self.has_bedrock_watchers() {
            World::broadcast_bedrock_grouped(
                be_packet,
                seen_by.iter().filter_map(|p| match p.client.as_ref() {
                    ClientPlatform::Bedrock(be_client) => Some(be_client),
                    ClientPlatform::Java(_) => None,
                }),
            );
        }
    }

    pub fn send_to_tracking_players_and_self<P: ClientPacket + Sync>(
        &self,
        packet: &P,
        world: &World,
    ) {
        self.send_to_tracking_players(packet, world);
        if let Some(player) = self.entity.get_player() {
            player.try_send_client_packet(packet);
        }
    }

    pub fn send_to_tracking_players_and_self_editioned<
        J: ClientPacket + Sync,
        B: BClientPacket + Sync,
    >(
        &self,
        je_packet: &J,
        be_packet: &B,
        world: &World,
    ) {
        self.send_to_tracking_players_editioned(je_packet, be_packet, world);
        if let Some(player) = self.entity.get_player() {
            player.try_enqueue_packet_editioned(je_packet, be_packet);
        }
    }

    pub fn send_to_tracking_players_filtered<P: ClientPacket + Sync, F: Fn(&Player) -> bool>(
        &self,
        packet: &P,
        _world: &World,
        filter: F,
    ) {
        let seen_by = self.viewers();
        if seen_by.is_empty() {
            return;
        }
        World::broadcast_java_players(packet, seen_by.iter().filter(|p| filter(p)));
    }

    pub fn send_to_tracking_players_filtered_editioned<
        J: ClientPacket + Sync,
        B: BClientPacket + Sync,
        F: Fn(&Player) -> bool,
    >(
        &self,
        je_packet: &J,
        be_packet: &B,
        _world: &World,
        filter: F,
    ) {
        let seen_by = self.viewers();
        if seen_by.is_empty() {
            return;
        }
        if !self.has_bedrock_watchers() {
            World::broadcast_java_players(je_packet, seen_by.iter().filter(|p| filter(p)));
            return;
        }
        let mut java_recipients = Vec::new();
        let mut bedrock_recipients = Vec::new();
        for p in seen_by.iter().filter(|p| filter(p)) {
            match p.client.as_ref() {
                ClientPlatform::Java(java_client) => java_recipients.push(java_client),
                ClientPlatform::Bedrock(be_client) => bedrock_recipients.push(be_client),
            }
        }
        World::broadcast_java_clients(je_packet, java_recipients.into_iter());
        World::broadcast_bedrock_grouped(be_packet, bedrock_recipients.into_iter());
    }
}

pub struct EntityTracker {
    pub entity_map: DashMap<i32, Arc<TrackedEntity>>,
}

impl Default for EntityTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl EntityTracker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            entity_map: DashMap::new(),
        }
    }

    #[must_use]
    pub fn get_tracked_entity(&self, entity_id: i32) -> Option<Arc<TrackedEntity>> {
        self.entity_map.get(&entity_id).map(|r| r.value().clone())
    }

    #[must_use]
    pub fn has_entity_with_id(&self, entity_id: i32) -> bool {
        self.entity_map.contains_key(&entity_id)
    }

    #[must_use]
    pub fn is_tracked_by_any_player(&self, entity_id: i32) -> bool {
        self.entity_map
            .get(&entity_id)
            .is_some_and(|t| t.has_viewers())
    }

    // Packet callbacks may add or remove tracked entities.
    fn tracked_entities(&self) -> Vec<Arc<TrackedEntity>> {
        self.entity_map
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    pub fn for_each_entity_tracked_by<F: FnMut(&Arc<dyn EntityBase>)>(
        &self,
        player: &Player,
        mut f: F,
    ) {
        for tracked in self.tracked_entities() {
            if !tracked.entity.get_entity().is_removed()
                && tracked.is_seen_by(&player.gameprofile.id)
            {
                f(&tracked.entity);
            }
        }
    }

    pub fn add_entity(&self, entity: &Arc<dyn EntityBase>, world: &World) {
        let base = entity.get_entity();
        if base.entity_type.client_tracking_range == 0 {
            return;
        }
        let tracked = self
            .entity_map
            .entry(base.entity_id)
            .and_modify(|tracked| {
                if !Arc::ptr_eq(&tracked.entity, entity) {
                    *tracked = Self::new_tracked_entity(entity);
                }
            })
            .or_insert_with(|| Self::new_tracked_entity(entity))
            .clone();
        let players = world.players.load();
        tracked.update_players(players.as_ref(), world);
    }

    fn new_tracked_entity(entity: &Arc<dyn EntityBase>) -> Arc<TrackedEntity> {
        let kind = entity.get_entity().entity_type;
        Arc::new(TrackedEntity::new(
            entity.clone(),
            kind.client_tracking_range,
            kind.update_interval,
            kind.track_deltas,
        ))
    }

    pub(super) fn try_publish_restored_drop(
        &self,
        world: &World,
        entity: &Arc<dyn EntityBase>,
        chunk: Vector2<i32>,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
    ) {
        let base = entity.get_entity();
        base.try_with_unmounted_actor(|| {
            if !base.is_leashed() && base.velocity.load().length_squared() <= 1e-4 {
                self.try_track_regional_actor(world, entity, chunk, None, java_recipients);
            }
        });
    }

    pub(super) fn try_publish_new_drop(
        &self,
        world: &World,
        entity: &Arc<dyn EntityBase>,
        chunk: Vector2<i32>,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
    ) {
        let base = entity.get_entity();
        base.try_with_unmounted_actor(|| {
            if !base.is_leashed() && base.first_tick.load(Relaxed) {
                self.try_track_regional_actor(world, entity, chunk, None, java_recipients);
            }
        });
    }

    pub(super) fn try_publish_restored_mob(
        &self,
        world: &World,
        entity: &Arc<dyn EntityBase>,
        chunk: Vector2<i32>,
        spawn: &RestoredMobSpawn,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
    ) {
        let base = entity.get_entity();
        let Some(living) = entity.get_living_entity() else {
            return;
        };
        let (Ok(vehicle), Ok(passengers), Ok(leash)) = (
            base.vehicle.try_lock(),
            base.passengers.try_lock(),
            base.leashed_to.try_lock(),
        ) else {
            return;
        };
        let (Ok(equipment), Ok(effects)) = (
            living.entity_equipment.try_lock(),
            living.active_effects.try_lock(),
        ) else {
            return;
        };
        if vehicle.is_some()
            || !passengers.is_empty()
            || leash.is_some()
            || !effects.is_empty()
            || equipment.equipment.values().any(|stack| !stack.is_empty())
            || base.velocity.load().length_squared() > 1e-4
            || !spawn.is_fresh(base, living.health.load())
        {
            return;
        }
        self.try_track_regional_actor(world, entity, chunk, Some(spawn), java_recipients);
    }

    fn try_track_regional_actor(
        &self,
        world: &World,
        entity: &Arc<dyn EntityBase>,
        chunk: Vector2<i32>,
        mob: Option<&RestoredMobSpawn>,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
    ) {
        let base = entity.get_entity();
        if base.is_removed()
            || base.chunk_pos.load() != chunk
            || !std::ptr::eq(base.world.load().as_ref(), world)
        {
            return;
        }
        let Some(Entry::Vacant(slot)) = self.entity_map.try_entry(base.entity_id) else {
            return;
        };
        let tracked = mob.map_or_else(
            || Self::new_tracked_entity(entity),
            |spawn| spawn.tracked.clone(),
        );
        slot.insert(tracked.clone());
        tracked.try_pair_restored_actor(world, chunk, mob, java_recipients);
    }

    /// Must only be called after the player's own `CLogin` packet has been sent.
    pub fn pair_new_player_with_tracked_entities(&self, player_arc: &Arc<Player>, world: &World) {
        let entity_id = player_arc.get_entity().entity_id;
        for tracked in self.tracked_entities() {
            if tracked.entity_id != entity_id {
                tracked.update_player(player_arc, world);
            }
        }
    }

    /// Vanilla `PlayerList.respawn` -> viewers drop the dead entity, then spawn it fresh.
    pub fn respawn_entity(&self, entity: &Arc<dyn EntityBase>, world: &World) {
        if let Some((_, tracked)) = self.entity_map.remove(&entity.get_entity().entity_id) {
            tracked.broadcast_removed(world);
        }
        self.add_entity(entity, world);
    }

    /// Vanilla `PlayerList.respawn` recreates the player -> forget and re-pair it as a viewer.
    pub fn repair_respawned_player(&self, player: &Arc<Player>, world: &World) {
        let entity_id = player.get_entity().entity_id;
        for tracked in self.tracked_entities() {
            if tracked.entity_id != entity_id {
                tracked.forget_player(player);
                tracked.update_player(player, world);
            }
        }
    }

    /// Pairs entities in chunks whose packet was just queued for `player`.
    pub fn update_player_chunks(
        &self,
        player: &Arc<Player>,
        world: &World,
        chunks: &[Vector2<i32>],
    ) {
        if chunks.is_empty() {
            return;
        }
        let chunks: FxHashSet<_> = chunks.iter().copied().collect();
        let entity_id = player.get_entity().entity_id;
        for tracked in self.tracked_entities() {
            if tracked.entity_id != entity_id
                && chunks.contains(&tracked.entity.get_entity().chunk_pos.load())
            {
                tracked.update_player(player, world);
            }
        }
    }

    pub fn remove_entity(&self, entity: &dyn EntityBase, world: &World) {
        let entity_id = entity.get_entity().entity_id;
        if let Some(player) = entity.get_player() {
            for tracked in self.tracked_entities() {
                tracked.remove_player(player);
            }
        }

        if let Some((_, tracked)) = self.entity_map.remove(&entity_id) {
            tracked.broadcast_removed(world);
        }
    }

    pub(super) fn try_publish_regional_drop_removals(
        &self,
        world: &World,
        server: &Server,
        ids: FxHashSet<i32>,
    ) {
        if server.plugin_manager.has_handlers::<PacketSentEvent>() {
            return;
        }
        let mut java_recipients = FxHashMap::default();
        for id in ids {
            let TryResult::Present(entry) = self.entity_map.try_get(&id) else {
                continue;
            };
            let tracked = entry.value().clone();
            drop(entry);
            tracked.try_publish_drop_removal(world, &mut java_recipients);
        }
        Self::flush_regional_java_packets(java_recipients);
    }

    pub(super) fn try_publish_regional_item_pickup(
        &self,
        world: &World,
        item: &Entity,
        collector: &Player,
        count: u8,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
        commit: impl FnOnce() -> Option<()>,
    ) -> Option<()> {
        let tracking = |entity: &Entity| match self.entity_map.try_get(&entity.entity_id) {
            TryResult::Present(entry)
                if std::ptr::eq(entry.value().entity.get_entity(), entity) =>
            {
                Some(entry.value().clone())
            }
            _ => None,
        };
        let chunk = item.chunk_pos.load();
        let item_tracking = tracking(item)?;
        let viewers = item_tracking.try_regional_move_viewers(chunk)?;
        let paired_collector = viewers.players.get(&collector.gameprofile.id)?;
        if !std::ptr::eq(paired_collector.as_ref(), collector) {
            return None;
        }
        let _ready = try_ready_regional_viewer(collector, world, chunk)?;
        let collector_tracking = tracking(collector.get_entity())?;
        let collector_viewers = collector_tracking.try_regional_pairings()?;
        // A pickup refers to two runtime IDs. Both spawns must precede the animation.
        if viewers.players.values().any(|viewer| {
            viewer.can_simulate_in(world)
                && !std::ptr::eq(viewer.as_ref(), collector)
                && collector_viewers
                    .players
                    .get(&viewer.gameprofile.id)
                    .is_none_or(|paired| !Arc::ptr_eq(paired, viewer))
        }) {
            return None;
        }
        Self::send_regional_to_viewers(
            world,
            None,
            Some(&viewers),
            java_recipients,
            || {
                let take = CTakeItemEntity::new(
                    item.entity_id.into(),
                    collector.entity_id().into(),
                    VarInt(i32::from(count)),
                );
                let java =
                    JavaClient::serialize_packet_for_version(&take, CURRENT_MC_VERSION).ok()?;
                let mut bedrock = Vec::new();
                BedrockClient::write_raw_packet(
                    &CTakeItemActor {
                        item_runtime_id: VarULong(item.entity_id as u64),
                        actor_runtime_id: VarULong(collector.entity_id() as u64),
                    },
                    &mut bedrock,
                )
                .ok()?;
                Some(RegionalActorPackets {
                    java: vec![java],
                    bedrock: vec![Bytes::from(bedrock)],
                    bedrock_viewer_equipment: None,
                })
            },
            commit,
        )?;
        Some(())
    }

    pub(super) fn try_publish_regional_effect_changes(
        &self,
        world: &World,
        server: &Server,
        subject: &dyn EntityBase,
        expired: &[&'static StatusEffect],
        data: RegionalEffectData,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
    ) -> Option<RegionalActorDelivery> {
        if server.plugin_manager.has_handlers::<PacketSentEvent>()
            || server
                .plugin_manager
                .has_handlers::<BedrockConnectionPacketSentEvent>()
            || (data.health.is_some()
                && server
                    .plugin_manager
                    .has_handlers::<EntityRegainHealthEvent>())
        {
            return None;
        }
        let entity = subject.get_entity();
        let owner = subject.get_player();
        let owner_health = match owner.zip(data.health) {
            Some((player, health)) => {
                let client = player.client.bedrock()?;
                let packet = if let Some((food, saturation)) = health.food {
                    player.bedrock_health_packet(health.health, food, saturation, health.max_health)
                } else {
                    bedrock_health_update(entity.entity_id, health.health, health.max_health)
                };
                let bytes = client.serialize_packet(&packet).ok()?;
                Some((player, client, bytes, health))
            }
            None => None,
        };
        let mut delivery = self.try_send_regional_actor_packets(
            world,
            subject,
            java_recipients,
            || Self::encode_regional_effect_changes(entity, expired, data),
            || Some(()),
        )?;
        if let Some((player, client, bytes, health)) = owner_health {
            let queued = client.try_enqueue_packet_data_unobserved(bytes);
            // A failed enqueue must be retried by the world HUD update, even if
            // the old and new health round to the same value.
            player
                .last_sent_health
                .store(if queued { health.health as i32 } else { -1 }, Relaxed);
            if let Some((food, saturation)) = health.food {
                if queued {
                    player.last_sent_food.store(food, Relaxed);
                    player
                        .last_food_saturation
                        .store(saturation == 0.0, Relaxed);
                } else {
                    client.mark_regional_food_dirty();
                }
            }
            delivery.bedrock_queued &= queued;
        }
        Some(delivery)
    }

    pub(crate) fn try_publish_regional_maturation(
        &self,
        world: &World,
        subject: &dyn EntityBase,
        metadata: SyncedActorDataList,
        java: Option<Box<[u8]>>,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
        apply: impl FnOnce() -> Option<()>,
    ) -> Option<RegionalActorDelivery> {
        let entity_id = subject.get_entity().entity_id;
        self.try_send_regional_actor_packets(
            world,
            subject,
            java_recipients,
            || {
                let java = java
                    .map(|bytes| {
                        JavaClient::serialize_packet_for_version(
                            &CSetEntityMetadata::new(entity_id.into(), bytes),
                            CURRENT_MC_VERSION,
                        )
                    })
                    .transpose()
                    .ok()?
                    .into_iter()
                    .collect();
                let packet = CSetActorData {
                    target_runtime_id: VarULong(entity_id as u64),
                    actor_data: metadata,
                    synced_properties: PropertySyncData::default(),
                    tick: VarULong(0),
                };
                let mut bedrock = Vec::new();
                BedrockClient::write_raw_packet(&packet, &mut bedrock).ok()?;
                Some(RegionalActorPackets {
                    java,
                    bedrock: vec![Bytes::from(bedrock)],
                    bedrock_viewer_equipment: None,
                })
            },
            apply,
        )
    }

    pub(super) fn try_publish_regional_player_metadata(
        &self,
        world: &World,
        player: &Player,
        metadata: SyncedActorDataList,
        java: Option<Box<[u8]>>,
        empty_main_hand: bool,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
    ) -> Option<RegionalActorDelivery> {
        self.try_send_regional_actor_packets(
            world,
            player,
            java_recipients,
            || {
                let entity_id = player.entity_id();
                let mut java_packets = match java {
                    Some(data) => {
                        let packet = CSetEntityMetadata::new(entity_id.into(), data);
                        vec![
                            JavaClient::serialize_packet_for_version(&packet, CURRENT_MC_VERSION)
                                .ok()?,
                        ]
                    }
                    None => Vec::new(),
                };
                let packet = CSetActorData {
                    target_runtime_id: VarULong(entity_id as u64),
                    actor_data: metadata,
                    synced_properties: PropertySyncData::default(),
                    tick: VarULong(0),
                };
                let mut bytes = Vec::new();
                BedrockClient::write_raw_packet(&packet, &mut bytes).ok()?;
                let bedrock_viewer_equipment = if empty_main_hand {
                    let (java, bedrock) =
                        Self::encode_regional_main_hand(player, ItemStack::EMPTY)?;
                    java_packets.push(java);
                    Some(bedrock)
                } else {
                    None
                };
                Some(RegionalActorPackets {
                    java: java_packets,
                    bedrock: vec![Bytes::from(bytes)],
                    bedrock_viewer_equipment,
                })
            },
            || Some(()),
        )
    }

    pub(crate) fn try_publish_regional_main_hand(
        &self,
        world: &World,
        player: &Player,
        stack: &ItemStack,
        apply: impl FnOnce() -> Option<()>,
    ) -> Option<()> {
        let mut java_recipients = FxHashMap::default();
        self.try_send_regional_actor_packets(
            world,
            player,
            &mut java_recipients,
            || {
                let (java, bedrock) = Self::encode_regional_main_hand(player, stack)?;
                Some(RegionalActorPackets {
                    java: vec![java],
                    bedrock: Vec::new(),
                    bedrock_viewer_equipment: Some(bedrock),
                })
            },
            apply,
        )?;
        Self::flush_regional_java_packets(java_recipients);
        Some(())
    }

    fn encode_regional_main_hand(player: &Player, stack: &ItemStack) -> Option<(Bytes, Bytes)> {
        let hand = vec![(
            EquipmentSlot::MAIN_HAND.discriminant(),
            ItemStackSerializer::from(stack.clone()),
        )];
        let packet = CSetEquipment::new(player.entity_id().into(), hand);
        let java = JavaClient::serialize_packet_for_version(&packet, CURRENT_MC_VERSION).ok()?;
        let packet = player.living_entity.bedrock_hand_equipment(stack, 0);
        let mut bedrock = Vec::new();
        BedrockClient::write_raw_packet(&packet, &mut bedrock).ok()?;
        Some((java, Bytes::from(bedrock)))
    }

    fn try_send_regional_actor_packets(
        &self,
        world: &World,
        subject: &dyn EntityBase,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
        prepare: impl FnOnce() -> Option<RegionalActorPackets>,
        apply: impl FnOnce() -> Option<()>,
    ) -> Option<RegionalActorDelivery> {
        let entity = subject.get_entity();
        let tracked = match self.entity_map.try_get(&entity.entity_id) {
            TryResult::Present(entry) => Some(entry.value().clone()),
            TryResult::Absent => None,
            TryResult::Locked => return None,
        };
        let viewers = match &tracked {
            Some(tracked) if std::ptr::eq(tracked.entity.get_entity(), entity) => {
                Some(tracked.try_regional_pairings()?)
            }
            Some(_) => return None,
            None => None,
        };
        Self::send_regional_to_viewers(
            world,
            subject.get_player(),
            viewers.as_deref(),
            java_recipients,
            prepare,
            apply,
        )
    }

    fn send_regional_to_viewers(
        world: &World,
        owner: Option<&Player>,
        viewers: Option<&Viewers>,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
        prepare: impl FnOnce() -> Option<RegionalActorPackets>,
        apply: impl FnOnce() -> Option<()>,
    ) -> Option<RegionalActorDelivery> {
        let recipients = viewers
            .iter()
            .flat_map(|viewers| viewers.players.values().map(Arc::as_ref))
            .chain(owner)
            .filter(|player| player.can_simulate_in(world));
        if recipients
            .clone()
            .any(|player| match player.client.as_ref() {
                ClientPlatform::Java(client) => client.version.load() != CURRENT_MC_VERSION,
                ClientPlatform::Bedrock(client) => !client.is_current_protocol(),
            })
        {
            return None;
        }
        if recipients.clone().next().is_none() {
            apply()?;
            return Some(RegionalActorDelivery::DELIVERED);
        }

        let packets = prepare()?;
        apply()?;
        // Keep each pairing pinned through queuing so actor data cannot
        // overtake a new spawn or an existing despawn.
        let mut delivery = RegionalActorDelivery::DELIVERED;
        for player in recipients {
            match player.client.as_ref() {
                ClientPlatform::Java(client) => {
                    if client.try_enqueue_current_packet_batch(&packets.java) {
                        java_recipients.insert(client.id, player.client.clone());
                    } else {
                        delivery.java_queued = false;
                    }
                }
                ClientPlatform::Bedrock(client) => {
                    for packet in &packets.bedrock {
                        delivery.bedrock_queued &=
                            client.try_enqueue_packet_data_unobserved(packet.clone());
                    }
                    // The owner's hotbar is managed by its local UI and inventory packets.
                    if let Some(packet) = &packets.bedrock_viewer_equipment
                        && owner.is_none_or(|owner| !std::ptr::eq(owner, player))
                    {
                        delivery.bedrock_queued &=
                            client.try_enqueue_packet_data_unobserved(packet.clone());
                    }
                }
            }
        }
        Some(delivery)
    }

    fn encode_regional_effect_changes(
        entity: &Entity,
        expired: &[&'static StatusEffect],
        data: RegionalEffectData,
    ) -> Option<RegionalActorPackets> {
        let mut java_packets = Vec::with_capacity(expired.len() + usize::from(data.java.is_some()));
        let mut bedrock_packets = Vec::with_capacity(usize::from(data.bedrock_particles.is_some()));
        for effect in expired {
            let (java, mut bedrock) = World::mob_effect_removal_packets(entity.entity_id, effect);
            let java = JavaClient::serialize_packet_for_version(&java, CURRENT_MC_VERSION).ok()?;
            java_packets.push(java);
            if World::route_current_mob_effect(entity, effect, &mut bedrock) {
                let mut bytes = Vec::new();
                BedrockClient::write_raw_packet(&bedrock, &mut bytes).ok()?;
                bedrock_packets.push(Bytes::from(bytes));
            }
        }
        if let Some(metadata) = data.java {
            let packet = CSetEntityMetadata::new(entity.entity_id.into(), metadata);
            java_packets
                .push(JavaClient::serialize_packet_for_version(&packet, CURRENT_MC_VERSION).ok()?);
        }
        if let Some(packed) = data.bedrock_particles {
            let packet = BedrockEffectParticles::packet(entity.entity_id, packed);
            let mut bytes = Vec::new();
            BedrockClient::write_raw_packet(&packet, &mut bytes).ok()?;
            bedrock_packets.push(Bytes::from(bytes));
        }

        if entity.entity_type == &EntityType::HORSE
            && let Some(health) = data.health
        {
            let packet = bedrock_health_update(entity.entity_id, health.health, health.max_health);
            let mut bytes = Vec::new();
            BedrockClient::write_raw_packet(&packet, &mut bytes).ok()?;
            bedrock_packets.push(Bytes::from(bytes));
        }
        Some(RegionalActorPackets {
            java: java_packets,
            bedrock: bedrock_packets,
            bedrock_viewer_equipment: None,
        })
    }

    pub(super) fn try_publish_regional_actor_move(
        &self,
        world: &World,
        server: &Server,
        actor: &Arc<dyn EntityBase>,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
    ) {
        if server.plugin_manager.has_handlers::<PacketSentEvent>() {
            return;
        }
        let entity = actor.get_entity();
        let TryResult::Present(entry) = self.entity_map.try_get(&entity.entity_id) else {
            return;
        };
        let tracked = entry.value().clone();
        drop(entry);
        if Arc::ptr_eq(&tracked.entity, actor) {
            tracked.try_publish_actor_move(world, entity, java_recipients);
        }
    }

    /// Returns the recipients that already have the pose and whether every pairing was served.
    pub(crate) fn try_publish_regional_player_move<J: ClientPacket>(
        &self,
        world: &World,
        entity: &Entity,
        movement: &J,
        head: Option<&CHeadRot>,
        bedrock_move: &CMovePlayer,
    ) -> Option<(FxHashSet<i32>, bool)> {
        let TryResult::Present(entry) = self.entity_map.try_get(&entity.entity_id) else {
            return None;
        };
        let tracked = entry.value().clone();
        drop(entry);
        if entity.is_removed() || !std::ptr::eq(tracked.entity.get_entity(), entity) {
            return None;
        }
        let chunk = entity.chunk_pos.load();
        // Player steps use absolute positions. An existing pairing can receive
        // them before the world refreshes the tracked section, once terrain is ready.
        let viewers = tracked.try_regional_pairings()?;

        let mut received = FxHashSet::default();
        let mut java_packet = None;
        let mut bedrock_packet = None;
        for viewer in viewers.players.values() {
            let Some(_chunk) = try_ready_regional_viewer(viewer, world, chunk) else {
                continue;
            };
            let queued = match viewer.client.as_ref() {
                ClientPlatform::Java(client) if client.version.load() == CURRENT_MC_VERSION => {
                    let Some(packets) = java_packet
                        .get_or_insert_with(|| {
                            let mut packets = vec![
                                JavaClient::serialize_packet_for_version(
                                    movement,
                                    CURRENT_MC_VERSION,
                                )
                                .ok()?,
                            ];
                            if let Some(head) = head {
                                packets.push(
                                    JavaClient::serialize_packet_for_version(
                                        head,
                                        CURRENT_MC_VERSION,
                                    )
                                    .ok()?,
                                );
                            }
                            Some(packets)
                        })
                        .as_ref()
                    else {
                        continue;
                    };
                    let queued = client.try_enqueue_current_packet_batch(packets);
                    if queued {
                        client.flush_channel();
                    }
                    queued
                }
                ClientPlatform::Bedrock(client) if client.is_current_protocol() => bedrock_packet
                    .get_or_insert_with(|| client.serialize_packet(bedrock_move))
                    .as_ref()
                    .is_ok_and(|data| client.try_enqueue_packet_data_unobserved(data.clone())),
                _ => false,
            };
            if queued {
                received.insert(viewer.entity_id());
            }
        }
        let delivered = received.len() == viewers.players.len();
        Some((received, delivered))
    }

    pub(super) fn flush_regional_java_packets(
        java_recipients: FxHashMap<u64, Arc<ClientPlatform>>,
    ) {
        // The world may still have Java's ordinary tick flush suspended.
        // Release this batch once per connection.
        for recipient in java_recipients.into_values() {
            if let ClientPlatform::Java(client) = recipient.as_ref() {
                client.flush_channel();
            }
        }
    }

    pub fn update_player_position(&self, player: &Arc<Player>, _world: &World) {
        let player_id = player.get_entity().entity_id;
        if let Some(tracked) = self.entity_map.get(&player_id) {
            tracked.pos_dirty.store(true, Relaxed);
        }
    }

    pub(crate) fn try_mark_player_position(&self, player: &Player) -> Option<()> {
        let TryResult::Present(tracked) = self.entity_map.try_get(&player.entity_id()) else {
            return None;
        };
        if !std::ptr::eq(tracked.entity.get_entity(), player.get_entity()) {
            return None;
        }
        tracked.pos_dirty.store(true, Relaxed);
        Some(())
    }

    pub fn update_entity_position(&self, entity: &dyn EntityBase, world: &World) {
        if let Some(tracked) = self.get_tracked_entity(entity.get_entity().entity_id) {
            let new_pos = chunk_section_from_pos(&entity.get_entity().block_pos.load());
            tracked.last_section_pos.store(new_pos);
            let players = world.players.load();
            tracked.update_players(players.as_ref(), world);
        }
    }

    pub fn update_all(&self, world: &World) {
        let players = world.players.load();
        let all_snapshots = PlayerViews::new(
            players
                .iter()
                .map(|player| {
                    let entity = player.get_entity();
                    PlayerViewSnapshot {
                        player,
                        entity_id: entity.entity_id,
                        pos: entity.pos.load(),
                        watched: player.watched_section.load(),
                        view_distance: get_view_distance(player).get() as i32,
                    }
                })
                .collect(),
        );

        let tracked_entities = self.tracked_entities();

        let mut entity_moved = vec![false; tracked_entities.len()];
        let moved_player_ids: FxHashSet<i32> = tracked_entities
            .par_iter()
            .zip(entity_moved.par_iter_mut())
            .filter_map(|(tracked, moved_slot)| {
                let new_pos = chunk_section_from_pos(&tracked.entity.get_entity().block_pos.load());
                let old_pos = tracked.last_section_pos.load();
                let section_changed = old_pos != new_pos;
                let sub_section_moved = tracked.pos_dirty.swap(false, Relaxed);
                if !section_changed && !sub_section_moved {
                    None
                } else {
                    *moved_slot = true;
                    let chunk_changed = old_pos.x != new_pos.x || old_pos.z != new_pos.z;
                    tracked.update_players_from_snapshots(&all_snapshots, chunk_changed);
                    if section_changed {
                        tracked.last_section_pos.store(new_pos);
                    }
                    tracked.entity.get_player().map(|_| tracked.entity_id)
                }
            })
            .collect();

        if !moved_player_ids.is_empty() {
            let moved_snapshots = PlayerViews::new(
                all_snapshots
                    .players
                    .iter()
                    .copied()
                    .filter(|snap| moved_player_ids.contains(&snap.entity_id))
                    .collect(),
            );
            tracked_entities
                .par_iter()
                .zip(entity_moved.par_iter())
                .for_each(|(tracked, &moved)| {
                    if !moved {
                        tracked.update_players_from_snapshots(&moved_snapshots, false);
                    }
                });
        }

        tracked_entities.par_iter().for_each(|tracked| {
            if tracked.entity.get_player().is_none() {
                tracked.send_changes(world);
            } else if tracked.entity.get_entity().synched_data.is_dirty() {
                tracked.entity.get_entity().send_dirty_entity_data();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separated_views_keep_outgoing_viewers_in_roster_order() {
        let index = PlayerViewIndex::new((0..20).map(|offset| {
            (
                Uuid::from_u128(offset as u128 + 1),
                Vector3::new(16.0, 0.0, f64::from(offset - 8) * 128.0),
                8,
            )
        }))
        .unwrap();
        let nearby = index.nearby(Vector3::new(16.0, 64.0, 0.0), 16).unwrap();
        let paired = [Uuid::from_u128(20), Uuid::from_u128(9), Uuid::from_u128(1)];
        assert_eq!(
            index.ordered_candidates(nearby, paired.into_iter()),
            [0, 7, 8, 9, 19]
        );
    }

    #[test]
    fn dense_player_views_use_the_full_roster() {
        let index = PlayerViewIndex::new((0..20).map(|offset| {
            (
                Uuid::from_u128(offset as u128 + 1),
                Vector3::new(f64::from(offset), 0.0, 0.0),
                4,
            )
        }))
        .unwrap();
        assert!(index.nearby(Vector3::new(10.0, 64.0, 0.0), 8).is_none());
    }

    #[tokio::test]
    async fn world_pairing_reuses_a_tracker_created_by_a_region() {
        let folder = tempfile::tempdir().unwrap();
        let world = crate::entity::ai::test_support::world(&folder);
        let position = Vector3::new(8.0, 64.0, 8.0);
        let chunk = Vector2::new(0, 0);
        let actor = crate::entity::r#type::from_type(
            &EntityType::EXPERIENCE_ORB,
            position,
            &world,
            Uuid::new_v4(),
        );
        let id = actor.get_entity().entity_id;
        let mut recipients = FxHashMap::default();
        world
            .entity_tracker
            .try_publish_restored_drop(&world, &actor, chunk, &mut recipients);
        let first = world.entity_tracker.get_tracked_entity(id).unwrap();
        world.entity_tracker.add_entity(&actor, &world);
        let tracked = world.entity_tracker.get_tracked_entity(id).unwrap();
        assert!(Arc::ptr_eq(&first, &tracked));
        assert!(Arc::ptr_eq(&tracked.entity, &actor));
        assert!(recipients.is_empty());
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn restored_mob_pairing_checks_pose_and_reuses_the_prepared_tracker() {
        let folder = tempfile::tempdir().unwrap();
        let world = crate::entity::ai::test_support::world(&folder);
        let chunk = Vector2::new(0, 0);
        let actor = crate::entity::r#type::from_type(
            &EntityType::COW,
            Vector3::new(8.0, 64.0, 8.0),
            &world,
            Uuid::new_v4(),
        );
        let id = actor.get_entity().entity_id;
        let stale = RestoredMobSpawn::prepare(&actor).unwrap();
        actor.get_entity().set_pos(Vector3::new(9.0, 64.0, 8.0));
        let mut recipients = FxHashMap::default();
        world.entity_tracker.try_publish_restored_mob(
            &world,
            &actor,
            chunk,
            &stale,
            &mut recipients,
        );
        assert!(world.entity_tracker.get_tracked_entity(id).is_none());

        let spawn = RestoredMobSpawn::prepare(&actor).unwrap();
        world.entity_tracker.try_publish_restored_mob(
            &world,
            &actor,
            chunk,
            &spawn,
            &mut recipients,
        );
        world.entity_tracker.add_entity(&actor, &world);
        let tracked = world.entity_tracker.get_tracked_entity(id).unwrap();
        assert!(Arc::ptr_eq(&tracked, &spawn.tracked));
        assert!(recipients.is_empty());
        world.level.shutdown().await;
    }

    #[test]
    fn a_partial_regional_move_keeps_the_baseline_until_a_full_send() {
        let needs_sync = AtomicBool::new(false);
        let mut baseline = 0;
        for (position, queued, pending, expected) in
            [(1, 1, true, 0), (2, 0, true, 0), (3, 2, false, 3)]
        {
            RegionalMoveDelivery { viewers: 2, queued }.reconcile(&needs_sync, || {
                baseline = position;
            });
            assert_eq!(needs_sync.load(Relaxed), pending);
            assert_eq!(baseline, expected);
        }
    }
}
