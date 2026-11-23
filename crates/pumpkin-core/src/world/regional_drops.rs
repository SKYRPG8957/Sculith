use std::{
    any::Any,
    collections::hash_map::Entry,
    sync::{Arc, PoisonError},
};

use dashmap::try_result::TryResult;
use pumpkin_data::entity::EntityType;
use pumpkin_util::math::vector2::Vector2;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    entity::{
        Entity, EntityBase,
        experience_orb::ExperienceOrbEntity,
        item::{ItemEntity, RegionalItemStep},
    },
    server::{Server, TickDeadline},
};

use super::{
    RegionId, RegionLayout, World, entity_tracker::EntityTracker,
    regional_campfires::RegionalEntityUpdate,
};

bitflags::bitflags! {
    #[derive(Clone, Copy, Default, PartialEq, Eq)]
    struct DropKinds: u8 {
        const ITEM = 1;
        const EXPERIENCE = 2;
    }
}

impl DropKinds {
    fn of(base: &Entity) -> Self {
        if base.entity_type == &EntityType::ITEM {
            Self::ITEM
        } else if base.entity_type == &EntityType::EXPERIENCE_ORB {
            Self::EXPERIENCE
        } else {
            Self::empty()
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct RegionalDrops {
    kinds: DropKinds,
    by_region: FxHashMap<RegionId, Vec<Arc<dyn EntityBase>>>,
    assigned: FxHashMap<i32, RegionId>,
}

impl RegionalDrops {
    pub(super) fn contains(&self, entity_id: i32) -> bool {
        self.assigned.contains_key(&entity_id)
    }

    pub(super) fn assign(&mut self, owner: RegionId, actor: Arc<dyn EntityBase>) {
        let base = actor.get_entity();
        self.kinds |= DropKinds::of(base);
        self.assigned.insert(base.entity_id, owner);
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
            .regional_drops
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if !published.kinds.is_empty() {
            let mut reassigned = RegionalDrops {
                kinds: published.kinds,
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
        // A dispatched owner reads the matching drop plan when it acquires its permit.
        let previous = self.publish_entity_chunk_layout(layout);
        drop(published);
        previous.transfer_regional_inputs(layout);
    }

    pub(super) fn assign_regional_drops(
        &self,
        server: &Server,
        tickable: &[(&Arc<dyn EntityBase>, Vector2<i32>)],
        regional: bool,
        previous: &Arc<RegionalDrops>,
    ) -> Arc<RegionalDrops> {
        let mut next = RegionalDrops::default();
        if regional {
            next.kinds.insert(DropKinds::EXPERIENCE);
            next.kinds
                .set(DropKinds::ITEM, ItemEntity::timers_unobserved(server));
            let layout = self.region_layout.load();
            for &(entity, _) in tickable {
                let base = entity.get_entity();
                if next.kinds.intersects(DropKinds::of(base))
                    && let Some(owner) = layout.owner(base.chunk_pos.load())
                    && owner != RegionLayout::INACTIVE
                {
                    next.assigned.insert(base.entity_id, owner);
                }
            }
        }
        if next.kinds == previous.kinds && next.assigned == previous.assigned {
            // A running region can keep its snapshot across an ordinary world tick.
            return self
                .regional_drops
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
            .regional_drops
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        // A spawn or movement may have updated the assignments during the scan.
        if !regional || Arc::ptr_eq(&published, previous) {
            *published = Arc::new(next);
        }
        published.clone()
    }

    pub(crate) fn move_drop_across_chunks(&self, entity: &Entity, commit_position: impl FnOnce()) {
        // A move can overlap a change of layout. Hold the same permit used to
        // publish that layout until the position and assignment agree.
        let mut published = self
            .regional_drops
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        commit_position();
        if !published.kinds.intersects(DropKinds::of(entity))
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
            .regional_drops
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if !published.kinds.is_empty() {
            *published = Arc::default();
        }
        let empty = Arc::<RegionLayout>::default();
        let previous = self.publish_entity_chunk_layout(&empty);
        drop(published);
        previous.transfer_regional_inputs(&empty);
    }

    pub(super) fn advance_regional_drops(
        &self,
        server: &Server,
        owner: RegionId,
        deadline: TickDeadline,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) {
        let Ok(published) = self.regional_drops.try_read() else {
            return;
        };
        let plan = published.clone();
        drop(published);
        let Some(actors) = plan.by_region.get(&owner) else {
            return;
        };
        let item_timers = ItemEntity::timers_unobserved(server);
        let native = server.plugin_manager.try_without_native_code();
        let mut expired = Vec::new();
        let mut java_recipients = FxHashMap::default();
        for actor in actors {
            let base = actor.get_entity();
            let chunk = base.chunk_pos.load();
            if base.is_removed() || !owns(chunk) {
                continue;
            }
            // Pin residency and the current assignment through timers and motion.
            let Ok(_snapshot) = self.entity_chunk_changes.snapshot.try_read() else {
                break;
            };
            let Ok(published) = self.regional_drops.try_read() else {
                break;
            };
            if published.assigned.get(&base.entity_id) != Some(&owner) {
                continue;
            }
            let publish = || {
                if owns(base.chunk_pos.load()) {
                    self.entity_tracker.try_publish_regional_drop_move(
                        self,
                        server,
                        actor,
                        &mut java_recipients,
                    );
                }
            };
            let actor_data = actor.as_ref() as &dyn Any;
            if item_timers && let Some(item) = actor_data.downcast_ref::<ItemEntity>() {
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
                    RegionalItemStep::Advanced if native.is_some() => {
                        item.try_regional_motion(self, chunk, owner, deadline, owns, publish);
                    }
                    RegionalItemStep::Expired => expired.push(actor),
                    _ => {}
                }
            } else if native.is_some()
                && let Some(orb) = actor_data.downcast_ref::<ExperienceOrbEntity>()
                && let Some(_checkpoint) = server.try_regional_entity_save_guard()
            {
                orb.try_regional_orb_tick(self, chunk, owner, deadline, owns, publish);
                if orb.has_expired() {
                    expired.push(actor);
                }
            }
        }
        EntityTracker::flush_regional_drop_packets(java_recipients);
        drop(native);
        if !expired.is_empty() {
            self.expire_regional_drops(server, owner, &expired, owns);
        }
    }

    fn expire_regional_drops(
        &self,
        server: &Server,
        owner: RegionId,
        actors: &[&Arc<dyn EntityBase>],
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
        let Ok(published) = self.regional_drops.try_read() else {
            return;
        };
        let Ok(mut pending) = self.regional_entity_updates.0.try_lock() else {
            return;
        };

        let mut discarded = FxHashSet::default();
        for &actor in actors {
            let base = actor.get_entity();
            let chunk = base.chunk_pos.load();
            if published.assigned.get(&base.entity_id) != Some(&owner) || !owns(chunk) {
                continue;
            }
            let TryResult::Present(_loaded) = self.level.loaded_chunks.try_get(&chunk) else {
                continue;
            };
            base.try_regional_drop_expiry(|| {
                let actor_data = actor.as_ref() as &dyn Any;
                let expired = actor_data.downcast_ref::<ItemEntity>().is_some_and(|item| {
                    item.has_expired() && ItemEntity::timers_unobserved(server)
                }) || actor_data
                    .downcast_ref::<ExperienceOrbEntity>()
                    .is_some_and(ExperienceOrbEntity::has_expired);
                if base.chunk_pos.load() == chunk
                    && std::ptr::eq(base.world.load().as_ref(), self)
                    && expired
                    && base.mark_discarded()
                {
                    discarded.insert(base.entity_id);
                    pending.push(RegionalEntityUpdate::Expired(actor.clone()));
                }
            });
        }
        if !discarded.is_empty() {
            // One registry update for the batch. Flush the viewers we can serve
            // without callbacks before the world drains their queued cleanup.
            self.entities.rcu(|current| {
                current
                    .iter()
                    .filter(|actor| !discarded.contains(&actor.get_entity().entity_id))
                    .cloned()
                    .collect::<Vec<_>>()
            });
            self.entity_tracker
                .try_publish_regional_drop_expiries(self, server, discarded);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        time::{Duration, Instant},
    };

    use pumpkin_config::{
        AdvancedConfiguration, BasicConfiguration, telemetry::TelemetryConfig, world::WorldPreset,
    };
    use pumpkin_data::{entity::EntityType, item::Item, item_stack::ItemStack};
    use pumpkin_nbt::NbtCompound;
    use pumpkin_util::math::vector3::Vector3;

    use crate::data::VanillaData;

    use super::*;

    #[tokio::test]
    async fn a_handoff_in_another_region_preserves_the_running_item_step() {
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
        advanced.networking.bedrock.online_mode = false;
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
        let home = Vector2::new(0, 0);
        let origin = Vector2::new(10, 0);
        let destination = Vector2::new(20, 0);
        let layout = Arc::new(RegionLayout::from_active(
            FxHashMap::from_iter([(home, 0), (origin, 1), (destination, 2)]),
            FxHashMap::from_iter([
                (0, [home].into()),
                (1, [origin].into()),
                (2, [destination].into()),
            ]),
        ));
        world.publish_region_layout(&layout);
        world.level.get_or_fetch_chunk(home, |_| ()).await;

        let position =
            |chunk: Vector2<i32>| Vector3::new(f64::from(chunk.x) * 16.0 + 8.0, 80.0, 8.0);
        let spawn = |chunk| -> Arc<dyn EntityBase> {
            let item = Arc::new(ItemEntity::new(
                Entity::new(world.clone(), position(chunk), &EntityType::ITEM),
                ItemStack::new(1, &Item::STONE),
            ));
            world.add_entity_silent(item.clone());
            item
        };
        let local = spawn(home);
        let crossing = spawn(origin);
        let previous = world.regional_drops.read().unwrap().clone();
        world.assign_regional_drops(
            &server,
            &[(&local, home), (&crossing, origin)],
            true,
            &previous,
        );

        let moved = Cell::new(false);
        let deadline = TickDeadline {
            at: Instant::now(),
            period: Duration::from_millis(50),
        };
        world.advance_regional_drops(&server, 0, deadline, &|chunk| {
            // Place the handoff after the running job has read its actor list.
            if !moved.replace(true) {
                crossing.get_entity().set_pos(position(destination));
            }
            layout.owner(chunk) == Some(0)
        });
        let current = world.regional_drops.read().unwrap().clone();
        assert_eq!(current.assigned[&crossing.get_entity().entity_id], 2);
        let mut data = NbtCompound::new();
        local.write_nbt(&mut data);
        assert_eq!(
            data.get_short("PickupDelay"),
            Some(i16::from(ItemEntity::DEFAULT_PICKUP_DELAY - 1))
        );
        server.shutdown().await;
    }
}
