//! Applies asynchronous entity chunk results under their simulation owners.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, PoisonError, TryLockError, Weak, atomic::Ordering::Relaxed},
};

use dashmap::try_result::TryResult;
use pumpkin_data::entity::EntityType;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_world::{
    chunk::{ChunkEntityData, io::Dirtiable},
    level::SyncEntityChunk,
};
use rustc_hash::{FxHashMap, FxHashSet};
use sculith_util::math::{position::BlockPos, vector2::Vector2, vector3::Vector3};
use uuid::Uuid;

use crate::{
    entity::{
        Entity, EntityBase,
        item::ItemEntity,
        r#type::{from_type, load_entity_from_nbt, self_and_passengers},
    },
    plugin::server::packet::PacketSentEvent,
    server::{Server, TickDeadline},
};

use super::{
    RegionId, RegionLayout, World, entity_save_records,
    entity_tracker::{EntityTracker, RestoredMobSpawn},
    regional_campfires::RegionalEntityUpdate,
    saved_chunk_pos,
};

const REGIONAL_CHUNKS_PER_STEP: usize = 4;
const REGIONAL_ACTORS_PER_CHUNK: usize = 16;
const PREPARED_CHUNK_LIMIT: usize = 16;

#[derive(Default)]
pub(super) struct EntityChunkChanges {
    pending: Mutex<PendingEntityChunks>,
    // Manual entity snapshots and residency transitions share the saved cache.
    pub(super) snapshot: tokio::sync::RwLock<()>,
}

type PendingEntityChunks = FxHashMap<Vector2<i32>, Option<Box<PreparedChunkMobs>>>;

/// Ready positions for one active extent. The pending table owns the work.
#[derive(Default)]
pub(super) struct RegionalEntityChunks(Mutex<ReadyEntityChunks>);

#[derive(Default)]
struct ReadyEntityChunks {
    order: VecDeque<Vector2<i32>>,
    queued: FxHashSet<Vector2<i32>>,
}

impl RegionalEntityChunks {
    fn offer(&self, pos: Vector2<i32>) {
        let mut ready = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if ready.queued.insert(pos) {
            ready.order.push_back(pos);
        }
    }

    fn has_work(&self) -> bool {
        self.0.try_lock().is_ok_and(|ready| !ready.order.is_empty())
    }

    fn take_ready(&self, pending: &PendingEntityChunks) -> Vec<Vector2<i32>> {
        let mut ready = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let mut batch = Vec::new();
        for _ in 0..REGIONAL_CHUNKS_PER_STEP {
            let Some(pos) = ready.order.pop_front() else {
                break;
            };
            ready.queued.remove(&pos);
            if pending.get(&pos).is_some_and(Option::is_none) {
                batch.push(pos);
            }
        }
        batch
    }

    pub(super) fn clear(&self) {
        let mut ready = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        ready.order.clear();
        ready.queued.clear();
    }
}

struct PreparedChunkMobs {
    chunk: Weak<ChunkEntityData>,
    actors: FxHashMap<Uuid, Arc<dyn EntityBase>>,
}

impl PreparedChunkMobs {
    fn take(&mut self, record: &NbtCompound, pos: Vector2<i32>) -> Option<Arc<dyn EntityBase>> {
        let (kind, uuid, position) = preparable_mob(record, pos)?;
        let base = self.actors.get(&uuid)?.get_entity();
        if base.entity_type != kind || base.pos.load() != position {
            return None;
        }
        self.actors.remove(&uuid)
    }
}

pub(super) type ChunkActor = (Vector2<i32>, Arc<dyn EntityBase>);

pub(super) struct EntityRegistration<'a> {
    active: &'a Mutex<FxHashSet<Uuid>>,
    uuid: Uuid,
}

impl Drop for EntityRegistration<'_> {
    fn drop(&mut self) {
        self.active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.uuid);
    }
}

fn regional_actor_position(
    record: &NbtCompound,
    chunk: Vector2<i32>,
) -> Option<(Vector3<f64>, bool)> {
    let id = record.get_string("id")?;
    if matches!(
        id.strip_prefix("minecraft:").unwrap_or(id),
        "armor_stand" | "item" | "experience_orb" | "marker"
    ) {
        return unlinked_saved_identity(record, chunk).map(|(_, position)| (position, false));
    }
    // Persistent livestock do not enter the world's natural spawn counts.
    if record.get_bool("PersistenceRequired") != Some(true) {
        return None;
    }
    preparable_mob(record, chunk).map(|(_, _, position)| (position, true))
}

fn unlinked_saved_identity(
    record: &NbtCompound,
    chunk: Vector2<i32>,
) -> Option<(Uuid, Vector3<f64>)> {
    if record.get("Team").is_some()
        || record
            .get_list("Passengers")
            .is_some_and(|riders| !riders.is_empty())
    {
        return None;
    }
    let uuid = record.get_uuid("UUID")?;
    let position = Entity::saved_position(record)?;
    (BlockPos::floored_v(position).chunk_position() == chunk).then_some((uuid, position))
}

fn preparable_mob(
    record: &NbtCompound,
    chunk: Vector2<i32>,
) -> Option<(&'static EntityType, Uuid, Vector3<f64>)> {
    let id = record.get_string("id")?;
    let kind = EntityType::from_name(id.strip_prefix("minecraft:").unwrap_or(id))?;
    // Only these built-in mobs have had their constructors and restoration
    // paths checked for regional use.
    if ![
        &EntityType::CHICKEN,
        &EntityType::COW,
        &EntityType::PIG,
        &EntityType::SHEEP,
    ]
    .contains(&kind)
        || record.get("leash").is_some()
    {
        return None;
    }
    let (uuid, position) = unlinked_saved_identity(record, chunk)?;
    Some((kind, uuid, position))
}

fn init_loaded_actor(entity: &dyn EntityBase) {
    match entity.get_mob() {
        Some(mob) => mob.mob_init_data_tracker(),
        None => entity.init_data_tracker(),
    }
    // The previous incarnation's residual velocity is stale.
    entity.get_entity().velocity.store(Vector3::default());
}

pub(super) fn save_chunk_entities(chunk: &ChunkEntityData, records: Vec<NbtCompound>) {
    if !chunk.live.load(Relaxed) && records.is_empty() {
        return;
    }
    let mut data = chunk.data.lock().unwrap_or_else(PoisonError::into_inner);
    store_entity_records(chunk, &mut data, records);
}

fn store_entity_records(
    chunk: &ChunkEntityData,
    data: &mut Vec<NbtCompound>,
    records: Vec<NbtCompound>,
) {
    let live = chunk.live.load(Relaxed);
    if live || !records.is_empty() {
        ChunkEntityData::merge_records(data, live, records);
        chunk.mark_dirty(true);
    }
}

impl World {
    pub(super) fn reserve_entity_registration(&self, uuid: Uuid) -> Option<EntityRegistration<'_>> {
        let active = &self.entity_index.registering;
        let mut pending = active.lock().unwrap_or_else(PoisonError::into_inner);
        if pending.contains(&uuid)
            || self
                .entities
                .load()
                .iter()
                .any(|entity| entity.get_entity().entity_uuid == uuid)
        {
            return None;
        }
        pending.insert(uuid);
        Some(EntityRegistration { active, uuid })
    }

    pub(crate) fn queue_entity_chunk_changes(
        &self,
        positions: impl IntoIterator<Item = Vector2<i32>>,
    ) {
        let mut pending = self
            .entity_chunk_changes
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let layout = self.region_layout.load();
        for pos in positions {
            if pending.entry(pos).or_insert(None).is_none()
                && let Some(ready) = layout.entity_chunk_work_at(pos)
            {
                ready.offer(pos);
            }
        }
    }

    pub(super) fn publish_entity_chunk_layout(
        &self,
        layout: &Arc<RegionLayout>,
    ) -> Arc<RegionLayout> {
        // Arrivals and regional commits check their layout under this lock.
        // Seed the new extents before publishing them, including positions
        // which had no active owner or were claimed by an old regional job.
        let pending = self
            .entity_chunk_changes
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let previous = self.region_layout.load();
        if layout.is_empty() {
            previous.clear_entity_chunk_work();
        } else {
            for (&pos, prepared) in pending.iter() {
                if prepared.is_none()
                    && let Some(ready) = layout.entity_chunk_work_at(pos)
                    && !previous
                        .entity_chunk_work_at(pos)
                        .is_some_and(|old| std::ptr::eq(old, ready))
                {
                    ready.offer(pos);
                }
            }
        }
        self.region_layout.swap(layout.clone())
    }

    pub(super) fn clear_prepared_chunk_mobs(&self) {
        for bundle in self
            .entity_chunk_changes
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values_mut()
        {
            *bundle = None;
        }
    }

    pub(super) fn request_entity_chunks(self: &Arc<Self>, positions: Vec<Vector2<i32>>) {
        let mut received = self.level.receive_entity_chunks(positions);
        let world = Arc::downgrade(self);
        self.level.spawn_task(async move {
            let mut batch = Vec::with_capacity(64);
            while received.recv_many(&mut batch, 64).await > 0 {
                let Some(world) = world.upgrade() else {
                    break;
                };
                world.queue_entity_chunk_changes(batch.iter().filter_map(|(weak, _)| {
                    let chunk = weak.upgrade()?;
                    Some(Vector2::new(chunk.x, chunk.z))
                }));
                batch.clear();
            }
        });
    }

    /// Drains work after movement, using the current cache and watchers for each position.
    pub(crate) fn apply_entity_chunk_changes(self: &Arc<Self>) {
        let Ok(snapshot) = self.entity_chunk_changes.snapshot.try_write() else {
            return;
        };
        let pending = {
            let mut changes = self
                .entity_chunk_changes
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if changes.is_empty() {
                return;
            }
            self.region_layout.load().clear_entity_chunk_work();
            std::mem::take(&mut *changes)
        };
        let active = self
            .active_chunks
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut retire = FxHashMap::default();
        let mut activate = Vec::new();
        for (pos, prepared) in pending {
            let Some(chunk) = self.level.get_entity_chunk_sync(&pos) else {
                continue;
            };
            if self.level.is_chunk_watched(&pos) {
                self.load_chunk_actors(pos, &chunk, prepared.map(|bundle| *bundle), &mut activate);
            } else if !active.contains(&pos) {
                retire.insert(pos, chunk);
            }
        }
        let retired = self.retire_chunk_entities(retire);
        let actors = self.register_chunk_actors(&activate);
        drop(snapshot);

        // Packet observers may request a save. The registry and saved cache
        // are already consistent before tracking can invoke their callbacks.
        self.finish_chunk_entity_removal(retired);
        self.publish_chunk_actors(actors);
    }

    pub(super) fn publish_chunk_actors(&self, actors: Vec<ChunkActor>) {
        if actors.is_empty() {
            return;
        }
        let players = self.players.load();
        for (pos, entity) in actors {
            if entity.get_entity().is_removed() {
                continue;
            }
            self.track_new_entity(&entity);
            self.finish_entity_registration(&entity);
            for player in players.iter().filter(|player| {
                player
                    .watched_section
                    .load()
                    .is_within_distance(pos.x, pos.y)
            }) {
                player.try_restore_vehicle(&entity);
            }
        }
    }

    pub(super) fn activate_regional_entity_chunks(
        self: &Arc<Self>,
        server: &Server,
        owner: RegionId,
        deadline: TickDeadline,
        ready: &RegionalEntityChunks,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) {
        if server.tick_rate_manager.is_frozen() || !ready.has_work() {
            return;
        }
        let Some(_native) = server.plugin_manager.try_without_native_code() else {
            return;
        };
        let Some(_checkpoint) = server.try_entity_save_guard() else {
            return;
        };
        let Ok(_snapshot) = self.entity_chunk_changes.snapshot.try_read() else {
            return;
        };
        let Ok(pending) = self.entity_chunk_changes.pending.try_lock() else {
            return;
        };
        let candidates = ready.take_ready(&pending);
        drop(pending);
        let item_timers = ItemEntity::timers_unobserved(server);
        for pos in candidates {
            if owns(pos)
                && !self.try_process_regional_chunk(pos, owner, deadline, item_timers, owns)
                && owns(pos)
            {
                // Retry after the other ready positions. A new extent will
                // find this position in the pending table during publication.
                ready.offer(pos);
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "saved records and live registrations commit together under the same guards"
    )]
    fn try_process_regional_chunk(
        self: &Arc<Self>,
        pos: Vector2<i32>,
        owner: RegionId,
        deadline: TickDeadline,
        item_timers: bool,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> bool {
        let Some(chunk) = self.level.try_with_watched_entity_chunk(pos, Arc::clone) else {
            return false;
        };
        let Ok(mut records) = chunk.data.try_lock() else {
            return false;
        };
        if chunk.live.load(Relaxed) || records.len() > REGIONAL_ACTORS_PER_CHUNK {
            return false;
        }
        let Some(positions) = records
            .iter()
            .map(|record| regional_actor_position(record, pos))
            .collect::<Option<Vec<_>>>()
        else {
            return self.try_prepare_chunk_mobs(pos, &chunk, &records, owns);
        };
        let terrain = if positions.iter().any(|&(_, needs_terrain)| needs_terrain) {
            let TryResult::Present(loaded) = self.level.loaded_chunks.try_get(&pos) else {
                return false;
            };
            Some(Arc::clone(loaded.value()))
        } else {
            None
        };
        let schedules_actor = |entity: &dyn EntityBase| {
            let base = entity.get_entity();
            base.regional_effects.load(Relaxed)
                || base.entity_type == &EntityType::EXPERIENCE_ORB
                || item_timers && base.entity_type == &EntityType::ITEM
        };
        let can_pair = self
            .server
            .upgrade()
            .is_some_and(|server| !server.plugin_manager.has_handlers::<PacketSentEvent>());
        let mut actors = Vec::with_capacity(records.len());
        let mut mob_spawns = FxHashMap::default();
        for (record, (position, restored_mob)) in records.iter().zip(positions) {
            let Some(entity) = load_entity_from_nbt(record, position, self) else {
                return false;
            };
            if entity.get_entity().chunk_pos.load() != pos {
                return false;
            }
            init_loaded_actor(entity.as_ref());
            if can_pair
                && restored_mob
                && let Some(spawn) = RestoredMobSpawn::prepare(&entity)
            {
                mob_spawns.insert(entity.get_entity().entity_id, spawn);
            }
            if item_timers
                && let Some(item) =
                    (entity.as_ref() as &dyn std::any::Any).downcast_ref::<ItemEntity>()
            {
                item.start_regional_timer(owner, deadline);
            } else if entity.get_entity().entity_type == &EntityType::EXPERIENCE_ORB {
                entity
                    .get_entity()
                    .start_regional_drop_motion(owner, deadline);
            } else if entity.get_entity().regional_effects.load(Relaxed)
                && let Some(living) = entity.get_living_entity()
            {
                living.start_restored_regional_effects(deadline);
            }
            actors.push((pos, entity));
        }

        // Publish restored actors and their work together. Moves take the same assignment lock.
        let mut actor_plan = if actors
            .iter()
            .any(|(_, entity)| schedules_actor(entity.as_ref()))
        {
            let Ok(plan) = self.regional_actors.try_write() else {
                return false;
            };
            Some(plan)
        } else {
            None
        };
        let Ok(registering) = self.entity_index.registering.try_lock() else {
            return false;
        };
        if actors
            .iter()
            .any(|(_, entity)| registering.contains(&entity.get_entity().entity_uuid))
        {
            return false;
        }
        let Ok(mut updates) = self.regional_entity_updates.0.try_lock() else {
            return false;
        };
        let Ok(mut pending) = self.entity_chunk_changes.pending.try_lock() else {
            return false;
        };
        let Some(added) = self
            .level
            .try_with_watched_entity_chunk(pos, |current| {
                // A replacement terrain chunk can change a restored mob's biome.
                let _terrain = if let Some(terrain) = &terrain {
                    let TryResult::Present(loaded) = self.level.loaded_chunks.try_get(&pos) else {
                        return None;
                    };
                    if !Arc::ptr_eq(loaded.value(), terrain) {
                        return None;
                    }
                    Some(loaded)
                } else {
                    None
                };
                if !Arc::ptr_eq(current, &chunk) || !owns(pos) || pending.remove(&pos).is_none() {
                    return None;
                }
                let added = self.register_chunk_actors(&actors);
                if let Some(plan) = actor_plan.as_mut() {
                    for (_, actor) in &added {
                        if schedules_actor(actor.as_ref()) {
                            Arc::make_mut(plan).assign(owner, actor.clone());
                        }
                    }
                }
                records.clear();
                chunk.live.store(true, Relaxed);
                Some(added)
            })
            .flatten()
        else {
            return false;
        };
        if added.is_empty() {
            return true;
        }
        tracing::trace!(world = %self.uuid, ?pos, count = added.len(), "Restored saved entities on their region");
        let pairable: Vec<_> = added
            .iter()
            .filter(|(_, actor)| {
                let base = actor.get_entity();
                base.entity_type == &EntityType::ITEM
                    || base.entity_type == &EntityType::EXPERIENCE_ORB
                    || mob_spawns.contains_key(&base.entity_id)
            })
            .map(|(_, actor)| actor.clone())
            .collect();
        updates.push(RegionalEntityUpdate::Loaded(added));
        drop(pending);
        drop(updates);
        drop(registering);
        drop(actor_plan);
        drop(records);

        if can_pair
            && !pairable.is_empty()
            && let Some(server) = self.server.upgrade()
            && !server.plugin_manager.has_handlers::<PacketSentEvent>()
        {
            self.try_publish_restored_actors(pos, owns, &pairable, &mob_spawns);
        }
        true
    }

    fn try_publish_restored_actors(
        &self,
        pos: Vector2<i32>,
        owns: &impl Fn(Vector2<i32>) -> bool,
        actors: &[Arc<dyn EntityBase>],
        mob_spawns: &FxHashMap<i32, RestoredMobSpawn>,
    ) {
        let mut java_recipients = FxHashMap::default();
        for actor in actors {
            // A layout change takes the writer. Do not pair under a revoked owner.
            let Ok(_layout) = self.regional_actors.try_read() else {
                break;
            };
            if !owns(pos) {
                break;
            }
            if let Some(spawn) = mob_spawns.get(&actor.get_entity().entity_id) {
                self.entity_tracker.try_publish_restored_mob(
                    self,
                    actor,
                    pos,
                    spawn,
                    &mut java_recipients,
                );
            } else {
                self.entity_tracker.try_publish_restored_drop(
                    self,
                    actor,
                    pos,
                    &mut java_recipients,
                );
            }
        }
        EntityTracker::flush_regional_java_packets(java_recipients);
    }

    fn try_prepare_chunk_mobs(
        self: &Arc<Self>,
        pos: Vector2<i32>,
        chunk: &SyncEntityChunk,
        records: &[NbtCompound],
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> bool {
        let Ok(pending) = self.entity_chunk_changes.pending.try_lock() else {
            return false;
        };
        let can_prepare = |pending: &PendingEntityChunks| {
            pending.get(&pos).is_some_and(Option::is_none)
                && pending
                    .values()
                    .filter(|bundle| bundle.is_some())
                    .take(PREPARED_CHUNK_LIMIT)
                    .count()
                    < PREPARED_CHUNK_LIMIT
        };
        if !can_prepare(&pending) {
            return false;
        }
        drop(pending);

        let actors: FxHashMap<_, _> = records
            .iter()
            .filter_map(|record| preparable_mob(record, pos))
            .map(|(kind, uuid, position)| (uuid, from_type(kind, position, self, uuid)))
            .collect();
        if actors.is_empty() {
            return false;
        }

        let Ok(mut pending) = self.entity_chunk_changes.pending.try_lock() else {
            return false;
        };
        self.level.try_with_watched_entity_chunk(pos, |current| {
            if !Arc::ptr_eq(current, chunk) || !owns(pos) || !can_prepare(&pending) {
                return false;
            }
            let count = actors.len();
            pending.insert(
                pos,
                Some(Box::new(PreparedChunkMobs {
                    chunk: Arc::downgrade(chunk),
                    actors,
                })),
            );
            tracing::trace!(world = %self.uuid, ?pos, count, "Prepared saved mobs on their region");
            true
        }) == Some(true)
    }

    fn load_chunk_actors(
        self: &Arc<Self>,
        pos: Vector2<i32>,
        chunk: &SyncEntityChunk,
        prepared: Option<PreparedChunkMobs>,
        actors: &mut Vec<ChunkActor>,
    ) {
        if chunk.live.load(Relaxed) {
            return;
        }
        let records = {
            let mut data = chunk.data.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(records) = self
                .level
                .try_with_watched_entity_chunk(pos, |current| {
                    Arc::ptr_eq(current, chunk).then(|| {
                        chunk.live.store(true, Relaxed);
                        std::mem::take(&mut *data)
                    })
                })
                .flatten()
            else {
                self.queue_entity_chunk_changes([pos]);
                return;
            };
            records
        };
        let mut prepared = prepared.filter(|bundle| bundle.chunk.as_ptr() == Arc::as_ptr(chunk));
        for record in &records {
            let root = if let Some(entity) = prepared
                .as_mut()
                .and_then(|bundle| bundle.take(record, pos))
            {
                let base = entity.get_entity();
                let biome = self.level.get_rough_biome(&base.block_pos.load());
                base.current_biome.store(Arc::new(biome));
                entity.read_nbt_non_mut(record);
                entity
            } else {
                let Some(entity) = load_entity_from_nbt(record, Vector3::default(), self) else {
                    continue;
                };
                entity
            };
            for entity in self_and_passengers(&root) {
                init_loaded_actor(entity.as_ref());
                actors.push((pos, entity));
            }
        }
    }

    fn register_chunk_actors(&self, actors: &[ChunkActor]) -> Vec<ChunkActor> {
        let mut added = Vec::new();
        if !actors.is_empty() {
            self.entities.rcu(|current| {
                added.clear();
                let mut known: FxHashSet<_> = current
                    .iter()
                    .map(|entity| entity.get_entity().entity_uuid)
                    .collect();
                added.extend(
                    actors
                        .iter()
                        .filter(|(_, entity)| known.insert(entity.get_entity().entity_uuid))
                        .cloned(),
                );
                if added.is_empty() {
                    return current.clone();
                }
                let mut next = (**current).clone();
                next.extend(added.iter().map(|(_, actor)| actor.clone()));
                Arc::new(next)
            });
        }
        added
    }

    fn retire_chunk_entities(
        &self,
        chunks: FxHashMap<Vector2<i32>, SyncEntityChunk>,
    ) -> Vec<Arc<dyn EntityBase>> {
        if chunks.is_empty() {
            return Vec::new();
        }
        let Some(access) = self.level.try_entity_retirement() else {
            self.queue_entity_chunk_changes(chunks.into_keys());
            return Vec::new();
        };
        // NBT preparation can touch actor locks. Do it before pinning the view
        // so an arriving watcher does not have to wait for serialization.
        let mut by_chunk = FxHashMap::<_, Vec<_>>::default();
        for entity in self.entities.load().iter() {
            let pos = saved_chunk_pos(entity.as_ref());
            if chunks.contains_key(&pos) {
                by_chunk.entry(pos).or_default().push(entity.clone());
            }
        }

        let mut retired = Vec::new();
        let mut cleaned = Vec::new();
        let mut retry = Vec::new();
        for (pos, chunk) in chunks {
            let entities = by_chunk.remove(&pos).unwrap_or_default();
            let mut grouped = entity_save_records(&entities);
            let records = grouped.remove(&pos).unwrap_or_default();
            if !grouped.is_empty() {
                retry.push(pos);
                continue;
            }
            let mut data = match chunk.data.try_lock() {
                Ok(data) => data,
                Err(TryLockError::Poisoned(error)) => error.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    retry.push(pos);
                    continue;
                }
            };
            let active = self
                .active_chunks
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            // Only pin movement during validation and commit. NBT hooks above
            // may move an actor themselves and must be able to take the permit.
            let _placement = if entities.is_empty() {
                None
            } else {
                Some(match self.regional_actors.try_write() {
                    Ok(placement) => placement,
                    Err(TryLockError::Poisoned(error)) => error.into_inner(),
                    Err(TryLockError::WouldBlock) => {
                        retry.push(pos);
                        continue;
                    }
                })
            };
            if !active.contains(&pos)
                && entities.iter().all(|entity| {
                    let base = entity.get_entity();
                    !base.is_removed()
                        && std::ptr::eq(base.world.load().as_ref(), self)
                        && saved_chunk_pos(entity.as_ref()) == pos
                })
                && self.level.try_with_unwatched_entity_chunk(pos, &chunk, || {
                    store_entity_records(&chunk, &mut data, records);
                    for entity in &entities {
                        entity.get_entity().mark_unloaded_to_chunk();
                    }
                    chunk.live.store(false, Relaxed);
                })
            {
                retired.extend(entities);
                cleaned.push(pos);
            } else {
                retry.push(pos);
            }
        }

        if !retired.is_empty() {
            let ids: FxHashSet<_> = retired
                .iter()
                .map(|entity| entity.get_entity().entity_id)
                .collect();
            self.entities.rcu(|current| {
                current
                    .iter()
                    .filter(|entity| !ids.contains(&entity.get_entity().entity_id))
                    .cloned()
                    .collect::<Vec<_>>()
            });
        }
        self.level
            .clean_entity_chunks_with_retirement(cleaned, access);
        self.queue_entity_chunk_changes(retry);
        retired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_data::{effect::StatusEffect, item::Item, item_stack::ItemStack, potion::Effect};
    use sculith_config::{
        AdvancedConfiguration, BasicConfiguration, telemetry::TelemetryConfig, world::WorldPreset,
    };
    use std::{
        any::Any,
        sync::atomic::AtomicBool,
        time::{Duration, Instant},
    };

    use crate::{
        data::VanillaData,
        entity::{
            ai::test_support::world,
            decoration::armor_stand::ArmorStandEntity,
            item::RegionalItemStep,
            living::{LivingEntity, RegionalActorDelivery},
            passive::cow::CowEntity,
        },
        world::tick_regions::TickRegions,
    };

    struct RewatchedActor {
        base: Entity,
        rewatch: AtomicBool,
    }

    impl EntityBase for RewatchedActor {
        fn get_entity(&self) -> &Entity {
            &self.base
        }

        fn get_living_entity(&self) -> Option<&LivingEntity> {
            None
        }

        fn cast_any(&self) -> &dyn Any {
            self
        }

        fn write_custom_nbt(&self, _nbt: &mut NbtCompound) {
            if self.rewatch.swap(false, Relaxed) {
                self.base
                    .world
                    .load()
                    .level
                    .mark_chunks_as_newly_watched(&[self.base.chunk_pos.load()]);
            }
        }
    }

    struct RelocatingActor {
        base: Entity,
        destination: Vector3<f64>,
    }

    impl EntityBase for RelocatingActor {
        fn get_entity(&self) -> &Entity {
            &self.base
        }

        fn get_living_entity(&self) -> Option<&LivingEntity> {
            None
        }

        fn cast_any(&self) -> &dyn Any {
            self
        }

        fn write_custom_nbt(&self, _nbt: &mut NbtCompound) {
            self.base.set_pos(self.destination);
        }
    }

    #[tokio::test]
    async fn a_save_groups_an_actor_by_its_captured_position() {
        let folder = tempfile::tempdir().unwrap();
        let world = world(&folder);
        let position = Vector3::new(-0.5, 64.0, 8.5);
        let destination = Vector3::new(160.5, 64.0, 8.5);
        let actor: Arc<dyn EntityBase> = Arc::new(RelocatingActor {
            base: Entity::new(world.clone(), position, &EntityType::ARMOR_STAND),
            destination,
        });
        world.add_entity_silent(actor.clone());

        let records = entity_save_records(std::slice::from_ref(&actor));
        assert_eq!(actor.get_entity().pos.load(), destination);
        assert_eq!(records.len(), 1);
        let records = &records[&Vector2::new(-1, 0)];
        assert_eq!(records.len(), 1);
        assert_eq!(Entity::saved_position(&records[0]), Some(position));
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn a_watcher_during_capture_keeps_the_live_actor() {
        let folder = tempfile::tempdir().unwrap();
        let world = world(&folder);
        let pos = Vector2::new(10, 0);
        world.level.spawn_entity_generation(pos);
        let cached = world.level.get_entity_chunk_sync(&pos).unwrap();
        world.level.mark_chunks_as_newly_watched(&[pos]);
        world.queue_entity_chunk_changes([pos]);
        world.apply_entity_chunk_changes();

        let actor = Arc::new(RewatchedActor {
            base: Entity::new(
                world.clone(),
                Vector3::new(160.5, 64.0, 8.5),
                &EntityType::ARMOR_STAND,
            ),
            rewatch: AtomicBool::new(true),
        });
        let id = actor.base.entity_id;
        let uuid = actor.base.entity_uuid;
        world.add_entity_silent(actor.clone());

        world.queue_entity_chunk_changes(world.level.mark_chunks_as_not_watched([pos]));
        world.apply_entity_chunk_changes();
        assert!(world.level.is_chunk_watched(&pos));
        assert!(cached.live.load(Relaxed));
        assert!(cached.data.lock().unwrap().is_empty());
        assert!(!actor.base.is_removed());
        assert!(world.entity_tracker.has_entity_with_id(id));
        assert_eq!(
            world
                .get_entity_by_uuid(uuid)
                .unwrap()
                .get_entity()
                .entity_id,
            id
        );

        world.queue_entity_chunk_changes(world.level.mark_chunks_as_not_watched([pos]));
        world.apply_entity_chunk_changes();
        assert!(actor.base.is_removed());
        assert!(world.get_entity_by_uuid(uuid).is_none());
        let reloaded = world.level.get_entity_chunk(pos).await;
        let saved = reloaded.data.lock().unwrap().clone();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].get_uuid("UUID"), Some(uuid));
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn regional_restore_rechecks_watcher_owner_and_registration() {
        let folder = tempfile::tempdir().unwrap();
        let world = world(&folder);
        let pos = Vector2::new(10, 0);
        let position = Vector3::new(160.5, 64.0, 8.5);
        world.level.spawn_entity_generation(pos);
        let saved = world.level.get_entity_chunk_sync(&pos).unwrap();
        let stand = ArmorStandEntity::new(Entity::new(
            world.clone(),
            position,
            &EntityType::ARMOR_STAND,
        ));
        let uuid = stand.get_entity().entity_uuid;
        let mut record = NbtCompound::new();
        stand.write_nbt(&mut record);
        saved.data.lock().unwrap().push(record);
        world.queue_entity_chunk_changes([pos]);

        let snapshot = world.entity_chunk_changes.snapshot.try_write().unwrap();
        let owns = |chunk| chunk == pos;
        let deadline = TickDeadline {
            at: Instant::now(),
            period: Duration::from_millis(50),
        };
        assert!(!world.try_process_regional_chunk(pos, 0, deadline, false, &owns));
        world.level.mark_chunks_as_newly_watched(&[pos]);
        assert!(!world.try_process_regional_chunk(pos, 0, deadline, false, &|_| false));
        let reservation = world.reserve_entity_registration(uuid).unwrap();
        assert!(!world.try_process_regional_chunk(pos, 0, deadline, false, &owns));
        assert_eq!(saved.data.lock().unwrap().len(), 1);
        assert!(world.get_entity_by_uuid(uuid).is_none());
        drop(reservation);
        assert!(world.try_process_regional_chunk(pos, 0, deadline, false, &owns));
        assert!(!world.try_process_regional_chunk(pos, 0, deadline, false, &owns));
        drop(snapshot);

        let actor = world.get_entity_by_uuid(uuid).unwrap();
        let id = actor.get_entity().entity_id;
        assert_eq!(actor.get_entity().pos.load(), position);
        assert!(saved.live.load(Relaxed));
        assert!(saved.data.lock().unwrap().is_empty());
        assert!(!world.entity_tracker.has_entity_with_id(id));
        world.flush_regional_entity_updates();
        world.apply_entity_chunk_changes();
        assert!(world.entity_tracker.has_entity_with_id(id));
        assert_eq!(world.entities.load().len(), 1);
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn queued_entity_chunks_follow_their_owner_through_a_merge_and_split() {
        let folder = tempfile::tempdir().unwrap();
        let basic = BasicConfiguration {
            default_level_name: folder.path().join("world").to_string_lossy().into_owned(),
            allow_chat_reports: false,
            allow_nether: false,
            allow_end: false,
            ..Default::default()
        };
        let mut advanced = AdvancedConfiguration::default();
        advanced.world.generator = WorldPreset::Flat;
        let server = Server::new(
            basic,
            advanced,
            TelemetryConfig::default(),
            VanillaData::default(),
            Vec::new(),
        )
        .await
        .unwrap();
        let world = server.worlds.load()[0].clone();
        let west: Vec<_> = (10..16).map(|x| Vector2::new(x, 0)).collect();
        let bridge = Vector2::new(16, 0);
        let east = Vector2::new(17, 0);
        let watched: Vec<_> = west.iter().copied().chain([east]).collect();
        for &pos in &watched {
            world.level.spawn_entity_generation(pos);
        }
        world.level.mark_chunks_as_newly_watched(&watched);
        let cached = |pos| world.level.get_entity_chunk_sync(&pos).unwrap();
        let live = |pos| cached(pos).live.load(Relaxed);
        let stand = ArmorStandEntity::new(Entity::new(
            world.clone(),
            Vector3::new(280.5, 64.0, 8.5),
            &EntityType::ARMOR_STAND,
        ));
        let uuid = stand.get_entity().entity_uuid;
        let mut record = NbtCompound::new();
        stand.write_nbt(&mut record);
        cached(east).data.lock().unwrap().push(record);

        let mut regions = TickRegions::default();
        let mut publish = |active: &FxHashSet<_>| {
            regions.active_chunks_changed();
            regions.schedule_ticks(active, Vec::new(), false);
            let layout = regions.layout().clone();
            world.publish_region_layout(&layout, false);
            layout
        };
        let tick = |layout: &RegionLayout, pos| {
            let owner = layout.owner(pos).unwrap();
            world.advance_region_time(
                owner,
                layout.chunks(owner).unwrap(),
                TickDeadline {
                    at: Instant::now(),
                    period: Duration::from_millis(50),
                },
            );
        };
        let mut active = FxHashSet::from_iter(west.iter().copied());
        let initial = publish(&active);
        // East has a pending disk result before it has an active owner.
        world.queue_entity_chunk_changes(std::iter::once(west[0]).chain(watched));
        let advanced_with_locked_chunk = {
            let blocked = cached(west[0]);
            let _records = blocked.data.lock().unwrap();
            tick(&initial, west[0]);
            !live(west[0]) && west[1..4].iter().copied().all(live)
        };
        assert!(advanced_with_locked_chunk);
        assert!(!live(west[4]) && !live(west[5]) && !live(east));

        active.extend([bridge, east]);
        let merged = publish(&active);
        let saving = world.entity_chunk_changes.snapshot.try_write().unwrap();
        tick(&merged, east);
        drop(saving);
        active.remove(&bridge);
        let split = publish(&active);
        assert_ne!(split.owner(west[0]), split.owner(east));
        tick(&merged, east);
        assert!(!live(east));
        tick(&split, east);
        assert!(live(east));
        assert!(world.get_entity_by_uuid(uuid).is_some());
        assert!(!live(west[0]) && !live(west[4]));
        tick(&split, west[0]);
        assert!(west.iter().copied().all(live));

        world.queue_entity_chunk_changes([west[0]]);
        world.apply_entity_chunk_changes();
        world.flush_regional_entity_updates();
        assert!(!split.entity_chunk_work_at(west[0]).unwrap().has_work());
        assert_eq!(world.entities.load().len(), 1);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn regional_mob_preparation_reuses_matching_actors_with_the_latest_nbt() {
        let folder = tempfile::tempdir().unwrap();
        let world = world(&folder);
        let pos = Vector2::new(10, 0);
        world.level.spawn_entity_generation(pos);
        let saved = world.level.get_entity_chunk_sync(&pos).unwrap();
        let record = |kind, x| {
            let uuid = Uuid::new_v4();
            let actor = from_type(kind, Vector3::new(x, 64.0, 8.5), &world, uuid);
            let mut record = NbtCompound::new();
            actor.write_nbt(&mut record);
            (uuid, record)
        };
        let (cow, cow_record) = record(&EntityType::COW, 160.5);
        let (sheep, sheep_record) = record(&EntityType::SHEEP, 162.5);
        saved
            .data
            .lock()
            .unwrap()
            .extend([cow_record, sheep_record]);
        world.level.mark_chunks_as_newly_watched(&[pos]);
        world.queue_entity_chunk_changes([pos]);

        let snapshot = world.entity_chunk_changes.snapshot.try_read().unwrap();
        let deadline = TickDeadline {
            at: Instant::now(),
            period: Duration::from_millis(50),
        };
        let owns = |chunk| chunk == pos;
        assert!(!world.try_process_regional_chunk(pos, 0, deadline, false, &|_| false));
        assert!(world.try_process_regional_chunk(pos, 0, deadline, false, &owns));
        assert!(!world.try_process_regional_chunk(pos, 0, deadline, false, &owns));
        let (cow_id, sheep_id) = {
            let pending = world.entity_chunk_changes.pending.lock().unwrap();
            let prepared = &pending[&pos].as_ref().unwrap().actors;
            (
                prepared[&cow].get_entity().entity_id,
                prepared[&sheep].get_entity().entity_id,
            )
        };
        world.apply_entity_chunk_changes();
        assert!(world.get_entity_by_uuid(cow).is_none());
        assert!(!saved.live.load(Relaxed));
        assert!(!world.entity_tracker.has_entity_with_id(cow_id));

        saved.data.lock().unwrap()[0].put_float("Health", 6.0);
        saved.data.lock().unwrap()[1].put_list("Pos", vec![163.5.into(), 64.0.into(), 8.5.into()]);
        drop(snapshot);
        world.apply_entity_chunk_changes();

        let cow = world.get_entity_by_uuid(cow).unwrap();
        assert_eq!(cow.get_entity().entity_id, cow_id);
        assert_eq!(cow.get_living_entity().unwrap().health.load(), 6.0);
        assert!(world.entity_tracker.has_entity_with_id(cow_id));
        let sheep = world.get_entity_by_uuid(sheep).unwrap();
        assert_ne!(sheep.get_entity().entity_id, sheep_id);
        assert_eq!(
            sheep.get_entity().pos.load(),
            Vector3::new(163.5, 64.0, 8.5)
        );
        assert!(saved.live.load(Relaxed));
        assert!(saved.data.lock().unwrap().is_empty());
        assert_eq!(world.entities.load().len(), 2);
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn the_region_restores_a_persistent_mob_and_its_effect_clock_once() {
        let folder = tempfile::tempdir().unwrap();
        let world = world(&folder);
        let position = Vector3::new(160.5, 64.0, 8.5);
        let pos = BlockPos::floored_v(position).chunk_position();
        world.level.spawn_entity_generation(pos);
        world.level.mark_chunks_as_newly_watched(&[pos]);
        let chunk = world.level.get_entity_chunk_sync(&pos).unwrap();
        let uuid = Uuid::new_v4();
        let cow = from_type(&EntityType::COW, position, &world, uuid);
        let regeneration = &StatusEffect::REGENERATION;
        cow.get_living_entity().unwrap().add_effect(Effect {
            effect_type: regeneration,
            duration: 50,
            amplifier: 0,
            ambient: false,
            show_particles: false,
            show_icon: false,
            blend: false,
        });
        let mut record = NbtCompound::new();
        cow.write_nbt(&mut record);
        record.put_bool("PersistenceRequired", true);
        record.put_bool("AgeLocked", true);
        record.put_int("Age", -600);
        record.put_float("Health", 6.0);
        chunk.data.lock().unwrap().push(record);
        world.queue_entity_chunk_changes([pos]);

        let snapshot = world.entity_chunk_changes.snapshot.try_read().unwrap();
        let deadline = TickDeadline {
            at: Instant::now(),
            period: Duration::from_millis(50),
        };
        let owns = |chunk| chunk == pos;
        assert!(!world.try_process_regional_chunk(pos, 0, deadline, false, &owns));
        assert!(world.entity_chunk_changes.pending.lock().unwrap()[&pos].is_none());
        world.level.loaded_chunks.insert(
            pos,
            pumpkin_world::chunk::ChunkData::empty_sync(pos.x, pos.y),
        );
        assert!(!world.try_process_regional_chunk(pos, 0, deadline, false, &|_| false));
        assert!(world.get_entity_by_uuid(uuid).is_none());
        assert!(!chunk.live.load(Relaxed));
        assert_eq!(chunk.data.lock().unwrap().len(), 1);
        assert!(world.try_process_regional_chunk(pos, 0, deadline, false, &owns));
        assert!(chunk.live.load(Relaxed));
        assert!(chunk.data.lock().unwrap().is_empty());
        assert!(
            world
                .entity_chunk_changes
                .pending
                .lock()
                .unwrap()
                .is_empty()
        );

        let restored = world.get_entity_by_uuid(uuid).unwrap();
        let mob = restored.get_mob().unwrap();
        let age = mob.as_ageable().unwrap();
        let id = restored.get_entity().entity_id;
        assert_eq!(restored.get_entity().pos.load(), position);
        assert!(age.is_baby() && age.is_age_locked());
        assert_eq!(age.get_age(), -600);
        let living = &mob.get_mob_entity().living_entity;
        let state = || {
            (
                living.health.load(),
                living.get_effect(regeneration).unwrap().duration,
            )
        };
        assert!(world.regional_actors.read().unwrap().contains(id));
        assert_eq!(state(), (6.0, 50));
        assert!(!world.entity_tracker.has_entity_with_id(id));
        living.advance_regional_effects(
            deadline,
            || true,
            |_, _| panic!("step on restore deadline"),
        );
        assert_eq!(state(), (6.0, 50));
        let next = TickDeadline {
            at: deadline.at + deadline.period,
            ..deadline
        };
        living.advance_regional_effects(
            next,
            || true,
            |_, _| Some(RegionalActorDelivery::DELIVERED),
        );
        living.advance_regional_effects(next, || true, |_, _| panic!("duplicate step"));
        assert_eq!(state(), (7.0, 49));
        drop(snapshot);

        world.apply_entity_chunk_changes();
        world.flush_regional_entity_updates();
        assert!(world.entity_tracker.has_entity_with_id(id));
        assert_eq!(world.entities.load().len(), 1);
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn a_regional_restore_starts_drop_steps_on_the_next_deadline() {
        use crate::entity::experience_orb::ExperienceOrbEntity;

        let folder = tempfile::tempdir().unwrap();
        let world = world(&folder);
        let pos = Vector2::new(10, 0);
        world.level.loaded_chunks.insert(
            pos,
            pumpkin_world::chunk::ChunkData::empty_sync(pos.x, pos.y),
        );
        world.level.spawn_entity_generation(pos);
        let saved = world.level.get_entity_chunk_sync(&pos).unwrap();
        let item = ItemEntity::new(
            Entity::new(
                world.clone(),
                Vector3::new(160.5, 64.0, 8.5),
                &EntityType::ITEM,
            ),
            ItemStack::new(1, &Item::STONE),
        );
        let mut record = NbtCompound::new();
        item.write_nbt(&mut record);
        let orb_position = Vector3::new(168.5, 64.0, 8.5);
        let orb = ExperienceOrbEntity::new(
            Entity::new(world.clone(), orb_position, &EntityType::EXPERIENCE_ORB),
            17,
        );
        let mut orb_record = NbtCompound::new();
        orb.write_nbt(&mut orb_record);
        orb_record.put_short("Age", 5_999);
        saved.data.lock().unwrap().extend([record, orb_record]);
        world.level.mark_chunks_as_newly_watched(&[pos]);
        world.queue_entity_chunk_changes([pos]);

        let deadline = TickDeadline {
            at: Instant::now(),
            period: Duration::from_millis(50),
        };
        let snapshot = world.entity_chunk_changes.snapshot.try_write().unwrap();
        let owner = 5;
        let owns = |chunk| chunk == pos;
        assert!(world.try_process_regional_chunk(pos, owner, deadline, true, &owns));
        drop(snapshot);

        let restored = world
            .get_entity_by_uuid(item.get_entity().entity_uuid)
            .unwrap();
        let restored = (restored.as_ref() as &dyn std::any::Any)
            .downcast_ref::<ItemEntity>()
            .unwrap();
        let id = restored.get_entity().entity_id;
        assert!(world.regional_actors.read().unwrap().contains(id));
        assert!(!world.entity_tracker.has_entity_with_id(id));
        let next = TickDeadline {
            at: deadline.at + deadline.period,
            ..deadline
        };
        assert!(matches!(
            restored.advance_regional_timers(owner, deadline, || true),
            RegionalItemStep::Skipped
        ));
        assert!(matches!(
            restored.advance_regional_timers(owner, next, || true),
            RegionalItemStep::Advanced
        ));
        let restored_orb = world
            .get_entity_by_uuid(orb.get_entity().entity_uuid)
            .unwrap();
        let restored_orb = restored_orb
            .cast_any()
            .downcast_ref::<ExperienceOrbEntity>()
            .unwrap();
        let orb_id = restored_orb.get_entity().entity_id;
        assert!(world.regional_actors.read().unwrap().contains(orb_id));
        assert!(!world.entity_tracker.has_entity_with_id(orb_id));
        let mut moved = 0;
        let mut publish = || moved += 1;
        restored_orb.try_regional_orb_tick(&world, pos, owner, deadline, &owns, &mut publish);
        assert_eq!(restored_orb.get_entity().pos.load(), orb_position);
        restored_orb.try_regional_orb_tick(&world, pos, owner, next, &owns, &mut publish);
        restored_orb.try_regional_orb_tick(&world, pos, owner, next, &owns, &mut publish);
        assert_eq!(moved, 1);
        assert_eq!(
            restored_orb.get_entity().pos.load(),
            orb_position.add_raw(0.0, -0.03, 0.0)
        );
        let mut record = NbtCompound::new();
        restored_orb.write_nbt(&mut record);
        assert_eq!(record.get_short("Age"), Some(6_000));
        assert_eq!(record.get_short("Value"), Some(17));
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn a_crossing_actor_retires_only_at_the_new_chunk() {
        let folder = tempfile::tempdir().unwrap();
        let world = world(&folder);
        let source = Vector2::new(0, 0);
        let destination = Vector2::new(10, 0);
        for pos in [source, destination] {
            world.level.spawn_entity_generation(pos);
        }
        world
            .level
            .mark_chunks_as_newly_watched(&[source, destination]);
        world.queue_entity_chunk_changes([source, destination]);
        world.apply_entity_chunk_changes();

        let cow = CowEntity::new(Entity::new(
            world.clone(),
            Vector3::new(8.5, 64.0, 8.5),
            &EntityType::COW,
        ));
        let uuid = cow.get_entity().entity_uuid;
        world.add_entity_silent(cow.clone());

        world.queue_entity_chunk_changes(world.level.mark_chunks_as_not_watched([source]));
        let retirement_pending = {
            let _handoff = world.regional_actors.read().unwrap();
            world.apply_entity_chunk_changes();
            assert!(world.get_entity_by_uuid(uuid).is_some());
            let pending = world.entity_chunk_changes.pending.lock().unwrap();
            pending.contains_key(&source)
        };
        assert!(retirement_pending);
        cow.get_entity().set_pos(Vector3::new(160.5, 64.0, 8.5));
        world.apply_entity_chunk_changes();
        assert!(world.get_entity_by_uuid(uuid).is_some());
        assert!(world.level.get_entity_chunk_sync(&source).is_none());

        world.queue_entity_chunk_changes(world.level.mark_chunks_as_not_watched([destination]));
        world.level.mark_chunks_as_newly_watched(&[destination]);
        world.apply_entity_chunk_changes();
        assert!(world.get_entity_by_uuid(uuid).is_some());

        world.queue_entity_chunk_changes(world.level.mark_chunks_as_not_watched([destination]));
        world.apply_entity_chunk_changes();
        assert!(world.get_entity_by_uuid(uuid).is_none());
        assert!(world.level.get_entity_chunk_sync(&destination).is_none());

        assert!(
            world
                .level
                .get_entity_chunk(source)
                .await
                .data
                .lock()
                .unwrap()
                .is_empty()
        );
        let loaded = world.level.get_entity_chunk(destination).await;
        let (count, position) = {
            let saved = loaded.data.lock().unwrap();
            (saved.len(), saved.first().and_then(Entity::saved_position))
        };
        assert_eq!(count, 1);
        assert_eq!(position, Some(Vector3::new(160.5, 64.0, 8.5)));
        cow.get_entity().set_pos(Vector3::new(8.5, 64.0, 8.5));
        assert_eq!(cow.get_entity().chunk_pos.load(), destination);
        world.level.mark_chunks_as_newly_watched(&[destination]);
        world.queue_entity_chunk_changes([destination, destination]);
        world.apply_entity_chunk_changes();
        let restored = world.get_entity_by_uuid(uuid).unwrap();
        assert_eq!(restored.get_entity().chunk_pos.load(), destination);
        assert_eq!(world.entities.load().len(), 1);
        world.level.shutdown().await;
    }
}
