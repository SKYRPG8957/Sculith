use std::{
    any::Any,
    collections::hash_map::Entry,
    sync::{Arc, PoisonError},
};

use dashmap::try_result::TryResult;
use pumpkin_util::math::vector2::Vector2;
use rustc_hash::FxHashMap;

use crate::{
    entity::{Entity, EntityBase, item::ItemEntity},
    server::{Server, TickDeadline},
};

use super::{RegionId, RegionLayout, World};

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
    pub(super) fn assign_regional_item_timers(
        &self,
        server: &Server,
        tickable: &[(&Arc<dyn EntityBase>, Vector2<i32>)],
        regional: bool,
    ) -> Arc<RegionalItemTimers> {
        let previous = self
            .regional_item_timers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
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
        // A movement may have assigned a new owner while this plan was built.
        if !next.enabled || Arc::ptr_eq(&published, &previous) {
            *published = Arc::new(next);
        }
        published.clone()
    }

    pub(crate) fn move_item_between_regions(
        &self,
        entity: &Entity,
        origin: Vector2<i32>,
        destination: Vector2<i32>,
        commit_position: impl FnOnce(),
    ) {
        let layout = self.region_layout.load();
        if layout.owner(origin) == layout.owner(destination) {
            commit_position();
            return;
        }
        drop(layout);

        // A regional job holds a read permit while it touches the item. Keep
        // position and ownership in the same handoff under the write permit.
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
            timers.assigned.insert(id, owner);
            timers.by_region.entry(owner).or_default().push(actor);
        }
    }

    pub(super) fn clear_regional_item_timers(&self) {
        let mut published = self
            .regional_item_timers
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if published.enabled {
            *published = Arc::default();
        }
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
                return;
            };
            let Ok(published) = self.regional_item_timers.try_read() else {
                return;
            };
            if !Arc::ptr_eq(&published, &plan) {
                return;
            }
            let TryResult::Present(loaded) = self.level.loaded_chunks.try_get(&chunk) else {
                continue;
            };
            let can_move = item.advance_regional_timers(owner, deadline, || {
                !base.is_removed()
                    && std::ptr::eq(base.world.load().as_ref(), self)
                    && base.chunk_pos.load() == chunk
                    && owns(chunk)
            });
            drop(loaded);
            if can_move && let Some(_native) = server.plugin_manager.try_without_native_code() {
                item.try_regional_motion(self, chunk, owner, deadline, owns);
            }
        }
    }
}
