//! Applies asynchronous entity chunk results under their simulation owners.

use std::sync::{Arc, Mutex, PoisonError, TryLockError, atomic::Ordering::Relaxed};

use pumpkin_data::entity::EntityType;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_util::math::{position::BlockPos, vector2::Vector2, vector3::Vector3};
use pumpkin_world::{
    chunk::{ChunkEntityData, io::Dirtiable},
    level::SyncEntityChunk,
};
use rustc_hash::{FxHashMap, FxHashSet};
use uuid::Uuid;

use crate::{
    entity::{
        Entity, EntityBase,
        item::ItemEntity,
        r#type::{load_entity_from_nbt, self_and_passengers},
    },
    server::{Server, TickDeadline},
};

use super::{
    RegionId, World, entity_save_records, merge_entity_records,
    regional_campfires::RegionalEntityUpdate, saved_chunk_pos,
};

const REGIONAL_CHUNKS_PER_STEP: usize = 4;
const REGIONAL_ACTORS_PER_CHUNK: usize = 16;

#[derive(Default)]
pub(super) struct EntityChunkChanges {
    pending: Mutex<FxHashSet<Vector2<i32>>>,
    // Manual entity snapshots and residency transitions share the saved cache.
    pub(super) snapshot: tokio::sync::RwLock<()>,
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

fn regional_actor_position(record: &NbtCompound, chunk: Vector2<i32>) -> Option<Vector3<f64>> {
    let id = record.get_string("id")?;
    // These constructors restore local fields. Team and passenger loading can
    // still modify the scoreboard or introduce actors outside this extent.
    if !matches!(
        id.strip_prefix("minecraft:").unwrap_or(id),
        "armor_stand" | "item" | "experience_orb" | "marker"
    ) || record.get("Team").is_some()
        || record
            .get_list("Passengers")
            .is_some_and(|riders| !riders.is_empty())
    {
        return None;
    }
    record.get_uuid("UUID")?;
    let position = Entity::saved_position(record)?;
    (BlockPos::floored_v(position).chunk_position() == chunk).then_some(position)
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
        merge_entity_records(data, live, records);
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
        self.entity_chunk_changes
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(positions);
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
            std::mem::take(&mut *changes)
        };
        let active = self
            .active_chunks
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut retire = FxHashMap::default();
        let mut activate = Vec::new();
        for pos in pending {
            let Some(chunk) = self.level.get_entity_chunk_sync(&pos) else {
                continue;
            };
            if self.level.is_chunk_watched(&pos) {
                self.load_chunk_actors(pos, &chunk, &mut activate);
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
        extent: &[Vector2<i32>],
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) {
        let Ok(pending) = self.entity_chunk_changes.pending.try_lock() else {
            return;
        };
        if pending.is_empty() || server.tick_rate_manager.is_frozen() {
            return;
        }
        let candidates: Vec<_> = extent
            .iter()
            .copied()
            .filter(|pos| pending.contains(pos))
            .collect();
        drop(pending);
        if candidates.is_empty() {
            return;
        }
        let Some(_native) = server.plugin_manager.try_without_native_code() else {
            return;
        };
        let Some(_checkpoint) = server.try_regional_entity_save_guard() else {
            return;
        };
        let Ok(_snapshot) = self.entity_chunk_changes.snapshot.try_write() else {
            return;
        };
        let mut activated = 0;
        for pos in candidates {
            let item_timers = ItemEntity::timers_unobserved(server);
            if owns(pos)
                && self.try_activate_regional_chunk(pos, owner, deadline, item_timers, owns)
            {
                activated += 1;
                if activated == REGIONAL_CHUNKS_PER_STEP {
                    break;
                }
            }
        }
    }

    fn try_activate_regional_chunk(
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
            return false;
        };
        let schedules_drop = |entity: &dyn EntityBase| {
            let kind = entity.get_entity().entity_type;
            kind == &EntityType::EXPERIENCE_ORB || item_timers && kind == &EntityType::ITEM
        };
        let mut actors = Vec::with_capacity(records.len());
        for (record, position) in records.iter().zip(positions) {
            let Some(entity) = load_entity_from_nbt(record, position, self) else {
                return false;
            };
            if entity.get_entity().chunk_pos.load() != pos {
                return false;
            }
            init_loaded_actor(entity.as_ref());
            if item_timers
                && let Some(item) =
                    (entity.as_ref() as &dyn std::any::Any).downcast_ref::<ItemEntity>()
            {
                item.start_regional_timer(owner, deadline);
            } else if entity.get_entity().entity_type == &EntityType::EXPERIENCE_ORB {
                entity
                    .get_entity()
                    .start_regional_drop_motion(owner, deadline);
            }
            actors.push((pos, entity));
        }

        // Publish restored drops with their assignment before a world tick can
        // pick them up in its entity scan. Moves take the same assignment lock.
        let mut drop_plan = if actors
            .iter()
            .any(|(_, entity)| schedules_drop(entity.as_ref()))
        {
            let Ok(plan) = self.regional_drops.try_write() else {
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
        self.level.try_with_watched_entity_chunk(pos, |current| {
            if !Arc::ptr_eq(current, &chunk) || !owns(pos) || !pending.remove(&pos) {
                return false;
            }
            let added = self.register_chunk_actors(&actors);
            if let Some(plan) = drop_plan.as_mut() {
                for (_, actor) in &added {
                    if schedules_drop(actor.as_ref()) {
                        Arc::make_mut(plan).assign(owner, actor.clone());
                    }
                }
            }
            records.clear();
            chunk.live.store(true, Relaxed);
            if !added.is_empty() {
                tracing::trace!(world = %self.uuid, ?pos, count = added.len(), "Restored saved entities on their region");
                updates.push(RegionalEntityUpdate::Loaded(added));
            }
            true
        }) == Some(true)
    }

    fn load_chunk_actors(
        self: &Arc<Self>,
        pos: Vector2<i32>,
        chunk: &ChunkEntityData,
        actors: &mut Vec<ChunkActor>,
    ) {
        if chunk.live.load(Relaxed) {
            return;
        }
        let records = {
            let mut data = chunk.data.lock().unwrap_or_else(PoisonError::into_inner);
            chunk.live.store(true, Relaxed);
            std::mem::take(&mut *data)
        };
        for record in &records {
            let Some(root) = load_entity_from_nbt(record, Vector3::default(), self) else {
                continue;
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
            if !grouped.is_empty()
                || entities.iter().any(|entity| {
                    entity.get_entity().is_removed() || saved_chunk_pos(entity.as_ref()) != pos
                })
            {
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
            if !active.contains(&pos)
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
        self.level.clean_entity_chunks(cleaned);
        self.queue_entity_chunk_changes(retry);
        retired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_data::{item::Item, item_stack::ItemStack};
    use std::{
        any::Any,
        sync::atomic::AtomicBool,
        time::{Duration, Instant},
    };

    use crate::entity::{
        ai::test_support::world, decoration::armor_stand::ArmorStandEntity, item::RegionalItemStep,
        living::LivingEntity, passive::cow::CowEntity,
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
        assert!(!world.try_activate_regional_chunk(pos, 0, deadline, false, &owns));
        world.level.mark_chunks_as_newly_watched(&[pos]);
        assert!(!world.try_activate_regional_chunk(pos, 0, deadline, false, &|_| false));
        let reservation = world.reserve_entity_registration(uuid).unwrap();
        assert!(!world.try_activate_regional_chunk(pos, 0, deadline, false, &owns));
        assert_eq!(saved.data.lock().unwrap().len(), 1);
        assert!(world.get_entity_by_uuid(uuid).is_none());
        drop(reservation);
        assert!(world.try_activate_regional_chunk(pos, 0, deadline, false, &owns));
        assert!(!world.try_activate_regional_chunk(pos, 0, deadline, false, &owns));
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
    async fn a_regional_restore_starts_drop_steps_on_the_next_deadline() {
        use crate::entity::experience_orb::ExperienceOrbEntity;

        let folder = tempfile::tempdir().unwrap();
        let world = world(&folder);
        let pos = Vector2::new(10, 0);
        world.level.get_or_fetch_chunk(pos, |_| ()).await;
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
        assert!(world.try_activate_regional_chunk(pos, owner, deadline, true, &owns));
        drop(snapshot);

        let restored = world
            .get_entity_by_uuid(item.get_entity().entity_uuid)
            .unwrap();
        let restored = (restored.as_ref() as &dyn std::any::Any)
            .downcast_ref::<ItemEntity>()
            .unwrap();
        let id = restored.get_entity().entity_id;
        assert!(world.regional_drops.read().unwrap().contains(id));
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
        assert!(world.regional_drops.read().unwrap().contains(orb_id));
        assert!(!world.entity_tracker.has_entity_with_id(orb_id));
        restored_orb.try_regional_orb_tick(&world, pos, owner, deadline, &owns);
        assert_eq!(restored_orb.get_entity().pos.load(), orb_position);
        restored_orb.try_regional_orb_tick(&world, pos, owner, next, &owns);
        restored_orb.try_regional_orb_tick(&world, pos, owner, next, &owns);
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
        assert_eq!(loaded.data.lock().unwrap().len(), 1);
        world.level.mark_chunks_as_newly_watched(&[destination]);
        world.queue_entity_chunk_changes([destination, destination]);
        world.apply_entity_chunk_changes();
        let restored = world.get_entity_by_uuid(uuid).unwrap();
        assert_eq!(restored.get_entity().chunk_pos.load(), destination);
        assert_eq!(world.entities.load().len(), 1);
        world.level.shutdown().await;
    }
}
