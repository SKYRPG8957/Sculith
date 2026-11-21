use std::{
    any::Any,
    collections::hash_map::Entry,
    sync::{Arc, PoisonError},
};

use dashmap::try_result::TryResult;
use pumpkin_util::math::vector2::Vector2;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    entity::{
        Entity, EntityBase,
        item::{ItemEntity, RegionalItemStep},
    },
    server::{Server, TickDeadline},
};

use super::{RegionId, RegionLayout, World, regional_campfires::RegionalEntityUpdate};

#[derive(Clone, Default)]
pub(super) struct RegionalItemTimers {
    enabled: bool,
    by_region: FxHashMap<RegionId, Vec<Arc<dyn EntityBase>>>,
    assigned: FxHashMap<i32, RegionId>,
}

impl RegionalItemTimers {
    pub(super) fn contains(&self, entity_id: i32) -> bool {
        self.assigned.contains_key(&entity_id)
    }

    pub(super) fn assign(&mut self, owner: RegionId, actor: Arc<dyn EntityBase>) {
        self.enabled = true;
        self.assigned.insert(actor.get_entity().entity_id, owner);
        self.by_region.entry(owner).or_default().push(actor);
    }

    fn unassign(&mut self, entity_id: i32) -> Option<Arc<dyn EntityBase>> {
        let owner = self.assigned.remove(&entity_id)?;
        let Entry::Occupied(mut entry) = self.by_region.entry(owner) else {
            return None;
        };
        let actors = entry.get_mut();
        let index = actors
            .iter()
            .position(|actor| actor.get_entity().entity_id == entity_id)?;
        let actor = actors.swap_remove(index);
        if actors.is_empty() {
            entry.remove();
        }
        Some(actor)
    }
}

impl World {
    pub(super) fn publish_region_layout(&self, layout: &Arc<RegionLayout>) {
        let previous = self.region_layout.load();
        if Arc::ptr_eq(&previous, layout) {
            return;
        }
        if previous.shares_active_regions(layout) {
            self.region_layout.store(layout.clone());
            return;
        }

        let mut published = self
            .regional_item_timers
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if published.enabled {
            let mut reassigned = RegionalItemTimers {
                enabled: true,
                ..Default::default()
            };
            for actor in published.by_region.values().flatten() {
                let base = actor.get_entity();
                if !base.is_removed()
                    && std::ptr::eq(base.world.load().as_ref(), self)
                    && let Some(owner) = layout.owner(base.chunk_pos.load())
                    && owner != RegionLayout::INACTIVE
                {
                    reassigned.assign(owner, actor.clone());
                }
            }
            if reassigned.assigned != published.assigned {
                *published = Arc::new(reassigned);
            }
        }
        // A dispatched owner reads the matching item plan when it acquires its permit.
        self.region_layout.store(layout.clone());
    }

    pub(super) fn assign_regional_item_timers(
        &self,
        server: &Server,
        tickable: &[(&Arc<dyn EntityBase>, Vector2<i32>)],
        regional: bool,
        previous: &Arc<RegionalItemTimers>,
    ) -> Arc<RegionalItemTimers> {
        let mut next = RegionalItemTimers {
            enabled: regional && ItemEntity::timers_unobserved(server),
            ..Default::default()
        };
        if next.enabled {
            let layout = self.region_layout.load();
            for &(entity, _) in tickable {
                let base = entity.get_entity();
                if (entity.as_ref() as &dyn Any).is::<ItemEntity>()
                    && let Some(owner) = layout.owner(base.chunk_pos.load())
                    && owner != RegionLayout::INACTIVE
                {
                    next.assigned.insert(base.entity_id, owner);
                }
            }
        }
        if next.enabled == previous.enabled && next.assigned == previous.assigned {
            // A running region can keep its snapshot across an ordinary world tick.
            return self
                .regional_item_timers
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
        }
        for &(entity, _) in tickable {
            if let Some(&owner) = next.assigned.get(&entity.get_entity().entity_id) {
                next.by_region
                    .entry(owner)
                    .or_default()
                    .push(entity.clone());
            }
        }
        let mut published = self
            .regional_item_timers
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        // A spawn or movement may have updated the assignments during the scan.
        if !next.enabled || Arc::ptr_eq(&published, previous) {
            *published = Arc::new(next);
        }
        published.clone()
    }

    pub(crate) fn move_item_across_chunks(&self, entity: &Entity, commit_position: impl FnOnce()) {
        // A move can overlap a change of layout. Hold the same permit used to
        // publish that layout until the position and assignment agree.
        let mut published = self
            .regional_item_timers
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        commit_position();
        if !published.enabled
            || entity.is_removed()
            || !std::ptr::eq(entity.world.load().as_ref(), self)
        {
            return;
        }
        let owner = self
            .region_layout
            .load()
            .owner(entity.chunk_pos.load())
            .filter(|&owner| owner != RegionLayout::INACTIVE);
        let id = entity.entity_id;
        if published.assigned.get(&id).copied() == owner {
            return;
        }

        let timers = Arc::make_mut(&mut published);
        let previous = timers.unassign(id);
        if let Some(owner) = owner
            && let Some(actor) =
                previous.or_else(|| self.entity_index.by_id(&self.entities.load_full(), id))
            && std::ptr::eq(actor.get_entity(), entity)
        {
            timers.assign(owner, actor);
        }
    }

    pub(super) fn reset_region_layout(&self) {
        let mut published = self
            .regional_item_timers
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if published.enabled {
            *published = Arc::default();
        }
        self.region_layout.store(Arc::default());
    }

    pub(super) fn advance_regional_item_timers(
        &self,
        server: &Server,
        owner: RegionId,
        deadline: TickDeadline,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) {
        if !ItemEntity::timers_unobserved(server) {
            return;
        }
        let Ok(published) = self.regional_item_timers.try_read() else {
            return;
        };
        let plan = published.clone();
        drop(published);
        let Some(actors) = plan.by_region.get(&owner) else {
            return;
        };
        let mut expired = Vec::new();
        for actor in actors {
            let Some(item) = (actor.as_ref() as &dyn Any).downcast_ref::<ItemEntity>() else {
                continue;
            };
            let base = item.get_entity();
            let chunk = base.chunk_pos.load();
            if !owns(chunk) {
                continue;
            }
            // Pin residency and the current assignment through timers and motion.
            let Ok(_snapshot) = self.entity_chunk_changes.snapshot.try_read() else {
                break;
            };
            let Ok(published) = self.regional_item_timers.try_read() else {
                break;
            };
            if !Arc::ptr_eq(&published, &plan) {
                break;
            }
            let TryResult::Present(loaded) = self.level.loaded_chunks.try_get(&chunk) else {
                continue;
            };
            let step = item.advance_regional_timers(owner, deadline, || {
                !base.is_removed()
                    && std::ptr::eq(base.world.load().as_ref(), self)
                    && base.chunk_pos.load() == chunk
                    && owns(chunk)
            });
            drop(loaded);
            match step {
                RegionalItemStep::Advanced => {
                    if let Some(_native) = server.plugin_manager.try_without_native_code() {
                        item.try_regional_motion(self, chunk, owner, deadline, owns);
                    }
                }
                RegionalItemStep::Expired => expired.push((actor, item)),
                RegionalItemStep::Skipped => {}
            }
        }
        if !expired.is_empty() {
            self.expire_regional_items(server, owner, &expired, owns);
        }
    }

    fn expire_regional_items(
        &self,
        server: &Server,
        owner: RegionId,
        items: &[(&Arc<dyn EntityBase>, &ItemEntity)],
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) {
        let Some(_native) = server.plugin_manager.try_without_native_code() else {
            return;
        };
        let Some(_checkpoint) = server.try_regional_entity_save_guard() else {
            return;
        };
        let Ok(_snapshot) = self.entity_chunk_changes.snapshot.try_read() else {
            return;
        };
        let Ok(published) = self.regional_item_timers.try_read() else {
            return;
        };
        let Ok(mut pending) = self.regional_entity_updates.0.try_lock() else {
            return;
        };

        let mut discarded = FxHashSet::default();
        for &(actor, item) in items {
            let base = item.get_entity();
            let chunk = base.chunk_pos.load();
            if published.assigned.get(&base.entity_id) != Some(&owner) || !owns(chunk) {
                continue;
            }
            let TryResult::Present(_loaded) = self.level.loaded_chunks.try_get(&chunk) else {
                continue;
            };
            item.try_regional_expiry(|| {
                if base.chunk_pos.load() == chunk
                    && std::ptr::eq(base.world.load().as_ref(), self)
                    && ItemEntity::timers_unobserved(server)
                    && base.mark_discarded()
                {
                    discarded.insert(base.entity_id);
                    pending.push(RegionalEntityUpdate::Expired(actor.clone()));
                }
            });
        }
        if !discarded.is_empty() {
            // One registry update for the batch. Packet callbacks stay on the world owner.
            self.entities.rcu(|current| {
                current
                    .iter()
                    .filter(|actor| !discarded.contains(&actor.get_entity().entity_id))
                    .cloned()
                    .collect::<Vec<_>>()
            });
        }
    }
}
