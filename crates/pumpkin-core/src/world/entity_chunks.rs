//! Applies asynchronous entity chunk results under the world's simulation owner.

use std::sync::{Arc, Mutex, PoisonError, atomic::Ordering::Relaxed};

use pumpkin_nbt::compound::NbtCompound;
use pumpkin_util::math::{vector2::Vector2, vector3::Vector3};
use pumpkin_world::{
    chunk::{ChunkEntityData, io::Dirtiable},
    level::SyncEntityChunk,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::entity::{
    EntityBase,
    r#type::{load_entity_from_nbt, self_and_passengers},
};

use super::{World, entity_save_records, merge_entity_records};

#[derive(Default)]
pub(super) struct EntityChunkChanges {
    pending: Mutex<FxHashSet<Vector2<i32>>>,
    // Manual entity snapshots and residency transitions share the saved cache.
    pub(super) snapshot: tokio::sync::Mutex<()>,
}

type ChunkActor = (Vector2<i32>, Arc<dyn EntityBase>);

pub(super) fn save_chunk_entities(chunk: &ChunkEntityData, records: Vec<NbtCompound>) {
    let live = chunk.live.load(Relaxed);
    if !live && records.is_empty() {
        return;
    }
    let mut data = chunk.data.lock().unwrap_or_else(PoisonError::into_inner);
    merge_entity_records(&mut data, live, records);
    chunk.mark_dirty(true);
}

impl World {
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
    /// Only this phase changes entity residency for the normal chunk loader.
    pub(crate) fn apply_entity_chunk_changes(self: &Arc<Self>) {
        let Ok(snapshot) = self.entity_chunk_changes.snapshot.try_lock() else {
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
        if !actors.is_empty() {
            let players = self.players.load();
            for (pos, entity) in actors {
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
                match entity.get_mob() {
                    Some(mob) => mob.mob_init_data_tracker(),
                    None => entity.init_data_tracker(),
                }
                // The previous incarnation's residual velocity is stale.
                entity.get_entity().velocity.store(Vector3::default());
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
        let positions: FxHashSet<_> = chunks.keys().copied().collect();
        let entities = self.detach_chunk_entities(&positions);
        let mut records = entity_save_records(&entities);
        for (pos, chunk) in chunks {
            save_chunk_entities(&chunk, records.remove(&pos).unwrap_or_default());
            // If a watcher arrived during capture, this chunk stays cached. Its
            // next owner turn can reactivate the saved records from that cache.
            chunk.live.store(false, Relaxed);
        }
        self.level.clean_entity_chunks(positions);
        entities
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_data::entity::EntityType;

    use crate::entity::{Entity, ai::test_support::world, passive::cow::CowEntity};

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
