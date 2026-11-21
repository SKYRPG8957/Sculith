use std::{
    any::Any,
    sync::{Arc, PoisonError},
};

use dashmap::try_result::TryResult;
use pumpkin_util::math::vector2::Vector2;
use rustc_hash::FxHashMap;

use crate::{
    entity::{EntityBase, item::ItemEntity},
    server::{Server, TickDeadline},
};

use super::{RegionId, RegionLayout, World};

#[derive(Default)]
pub(super) struct RegionalItemTimers {
    by_region: FxHashMap<RegionId, Vec<Arc<dyn EntityBase>>>,
    assigned: FxHashMap<i32, RegionId>,
}

impl RegionalItemTimers {
    pub(super) fn contains(&self, entity_id: i32) -> bool {
        self.assigned.contains_key(&entity_id)
    }
}

impl World {
    pub(super) fn assign_regional_item_timers(
        &self,
        server: &Server,
        tickable: &[(&Arc<dyn EntityBase>, Vector2<i32>)],
        regional: bool,
    ) -> Arc<RegionalItemTimers> {
        let mut next = RegionalItemTimers::default();
        if regional && ItemEntity::timers_unobserved(server) {
            let layout = self.region_layout.load();
            for &(entity, chunk) in tickable {
                if (entity.as_ref() as &dyn Any).is::<ItemEntity>()
                    && let Some(owner) = layout.owner(chunk)
                    && owner != RegionLayout::INACTIVE
                {
                    next.assigned.insert(entity.get_entity().entity_id, owner);
                }
            }
        }
        {
            let published = self
                .regional_item_timers
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            // A running region can keep its snapshot across an ordinary world tick.
            if next.assigned == published.assigned {
                return published.clone();
            }
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
        *published = Arc::new(next);
        published.clone()
    }

    pub(super) fn clear_regional_item_timers(&self) {
        let mut published = self
            .regional_item_timers
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if !published.assigned.is_empty() {
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
            // Hold residency and the current assignment only for a timer mutation.
            let Ok(_snapshot) = self.entity_chunk_changes.snapshot.try_read() else {
                return;
            };
            let Ok(published) = self.regional_item_timers.try_read() else {
                return;
            };
            if !Arc::ptr_eq(&published, &plan) {
                return;
            }
            let TryResult::Present(_loaded) = self.level.loaded_chunks.try_get(&chunk) else {
                continue;
            };
            item.advance_regional_timers(owner, deadline, || {
                !base.is_removed()
                    && std::ptr::eq(base.world.load().as_ref(), self)
                    && base.chunk_pos.load() == chunk
                    && owns(chunk)
            });
        }
    }
}
