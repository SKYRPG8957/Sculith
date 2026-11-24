use std::{
    any::Any,
    cell::Cell,
    collections::hash_map::Entry,
    sync::{Arc, PoisonError, atomic::Ordering::Relaxed},
};

use dashmap::try_result::TryResult;
use pumpkin_data::entity::EntityType;
use rustc_hash::{FxHashMap, FxHashSet};
use sculith_util::math::vector2::Vector2;

use crate::{
    entity::{
        Entity, EntityBase,
        experience_orb::ExperienceOrbEntity,
        item::{ItemEntity, RegionalItemStep},
    },
    net::ClientPlatform,
    server::{Server, TickDeadline},
};

use super::{
    RegionId, RegionLayout, World, entity_tracker::EntityTracker,
    regional_campfires::RegionalEntityUpdate,
};

bitflags::bitflags! {
    #[derive(Clone, Copy, Default, PartialEq, Eq)]
    struct ActorWork: u8 {
        const ITEM = 1;
        const EXPERIENCE = 2;
        const PASSIVE_EFFECTS = 4;
    }
}

impl ActorWork {
    fn enabled(server: &Server) -> Self {
        let mut kinds = Self::EXPERIENCE | Self::PASSIVE_EFFECTS;
        kinds.set(Self::ITEM, ItemEntity::timers_unobserved(server));
        kinds
    }

    fn of(base: &Entity) -> Self {
        if base.entity_type == &EntityType::ITEM {
            Self::ITEM
        } else if base.entity_type == &EntityType::EXPERIENCE_ORB {
            Self::EXPERIENCE
        } else if base.regional_effects.load(Relaxed) {
            Self::PASSIVE_EFFECTS
        } else {
            Self::empty()
        }
    }
}

thread_local! {
    static ACTOR_DEADLINE: Cell<Option<(i32, TickDeadline)>> = const { Cell::new(None) };
}

pub fn actor_deadline(id: i32) -> Option<TickDeadline> {
    ACTOR_DEADLINE
        .get()
        .filter(|&(actor, _)| actor == id)
        .map(|(_, deadline)| deadline)
}

pub(super) fn tick_actor_with_deadline(
    id: i32,
    deadline: Option<TickDeadline>,
    tick: impl FnOnce(),
) {
    struct RestoreDeadline(Option<(i32, TickDeadline)>);
    impl Drop for RestoreDeadline {
        fn drop(&mut self) {
            ACTOR_DEADLINE.set(self.0);
        }
    }
    let _restore = RestoreDeadline(ACTOR_DEADLINE.replace(deadline.map(|step| (id, step))));
    tick();
}

#[derive(Clone, Default)]
pub(super) struct RegionalActors {
    kinds: ActorWork,
    by_region: FxHashMap<RegionId, Vec<Arc<dyn EntityBase>>>,
    assigned: FxHashMap<i32, RegionId>,
}

impl RegionalActors {
    pub(super) fn contains(&self, entity_id: i32) -> bool {
        self.assigned.contains_key(&entity_id)
    }

    pub(super) fn assign(&mut self, owner: RegionId, actor: Arc<dyn EntityBase>) {
        let base = actor.get_entity();
        self.kinds |= ActorWork::of(base);
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
    pub(super) fn publish_region_layout(&self, layout: &Arc<RegionLayout>, coordinated: bool) {
        let previous = self.region_layout.load();
        if Arc::ptr_eq(&previous, layout) {
            return;
        }
        if previous.shares_active_regions(layout) {
            self.region_layout.store(layout.clone());
            return;
        }
        let activates_chunks = layout.activates_chunks_from(&previous);

        let mut published = self
            .regional_actors
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if !coordinated && let Some(server) = self.server.upgrade() {
            let mut reassigned = RegionalActors {
                kinds: ActorWork::enabled(&server),
                ..Default::default()
            };
            for actor in published.by_region.values().flatten() {
                let base = actor.get_entity();
                if reassigned.kinds.intersects(ActorWork::of(base))
                    && !base.is_removed()
                    && std::ptr::eq(base.world.load().as_ref(), self)
                    && let Some(owner) = layout.active_owner(base.chunk_pos.load())
                {
                    reassigned.assign(owner, actor.clone());
                }
            }
            if activates_chunks {
                // An actor can stay loaded just outside simulation distance.
                // Pick up its work when the extent opens, before the world ticks actors.
                for actor in self.entities.load().iter() {
                    let base = actor.get_entity();
                    let pos = base.chunk_pos.load();
                    if reassigned.kinds.intersects(ActorWork::of(base))
                        && !base.is_removed()
                        && previous.active_owner(pos).is_none()
                        && let Some(owner) = layout.active_owner(pos)
                        && std::ptr::eq(base.world.load().as_ref(), self)
                        && !reassigned.contains(base.entity_id)
                    {
                        reassigned.assign(owner, actor.clone());
                    }
                }
            }
            if reassigned.kinds != published.kinds || reassigned.assigned != published.assigned {
                *published = Arc::new(reassigned);
            }
        }
        // A dispatched owner reads the matching drop plan when it acquires its permit.
        let previous = self.publish_entity_chunk_layout(layout);
        drop(published);
        previous.transfer_regional_inputs(layout);
        if activates_chunks {
            self.resume_regional_player_work(&previous, layout);
        }
    }

    pub(super) fn assign_regional_actors(
        &self,
        server: &Server,
        tickable: &[(&Arc<dyn EntityBase>, Vector2<i32>)],
        regional: bool,
        previous: &Arc<RegionalActors>,
    ) -> Arc<RegionalActors> {
        let mut next = RegionalActors::default();
        if regional {
            next.kinds = ActorWork::enabled(server);
            let layout = self.region_layout.load();
            for &(entity, _) in tickable {
                let base = entity.get_entity();
                if next.kinds.intersects(ActorWork::of(base))
                    && let Some(owner) = layout.active_owner(base.chunk_pos.load())
                {
                    next.assigned.insert(base.entity_id, owner);
                }
            }
        }
        if next.kinds == previous.kinds && next.assigned == previous.assigned {
            // A running region can keep its snapshot across an ordinary world tick.
            return self
                .regional_actors
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
            .regional_actors
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        // A spawn or movement may have updated the assignments during the scan.
        if !regional || Arc::ptr_eq(&published, previous) {
            *published = Arc::new(next);
        }
        published.clone()
    }

    pub(crate) fn move_actor_across_chunks(&self, entity: &Entity, commit_position: impl FnOnce()) {
        // Keep retirement and layout publication out until the position and assignment agree.
        let still_here =
            || !entity.is_removed() && std::ptr::eq(entity.world.load().as_ref(), self);
        let plan = self
            .regional_actors
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if !plan.kinds.intersects(ActorWork::of(entity)) && !plan.contains(entity.entity_id) {
            if still_here() {
                commit_position();
            }
            return;
        }
        drop(plan);
        let mut published = self
            .regional_actors
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if still_here() {
            commit_position();
            self.refresh_actor_owner(&mut published, entity);
        }
    }

    pub(crate) fn offer_mob_effects(&self, entity: &Entity) {
        let mut published = self
            .regional_actors
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if published.contains(entity.entity_id) {
            // A world scan may have seen the previous effect set become empty.
            Arc::make_mut(&mut published);
        }
        self.refresh_actor_owner(&mut published, entity);
    }

    fn refresh_actor_owner(&self, published: &mut Arc<RegionalActors>, entity: &Entity) {
        let layout = self.region_layout.load();
        let owner = if published.kinds.intersects(ActorWork::of(entity))
            && !entity.is_removed()
            && std::ptr::eq(entity.world.load().as_ref(), self)
        {
            layout.active_owner(entity.chunk_pos.load())
        } else {
            None
        };
        let id = entity.entity_id;
        if published.assigned.get(&id).copied() == owner {
            return;
        }

        let timers = Arc::make_mut(published);
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
            .regional_actors
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if !published.kinds.is_empty() {
            *published = Arc::default();
        }
        let empty = Arc::<RegionLayout>::default();
        let previous = self.publish_entity_chunk_layout(&empty);
        drop(published);
        previous.transfer_regional_inputs(&empty);
        // A resumed world must replace its retired queues even when players
        // have stayed in the same chunks during a freeze.
        self.active_chunk_tracker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tick_regions
            .active_chunks_changed();
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Orders actor steps and their packets under the same assignment guards"
    )]
    pub(super) fn advance_regional_actors(
        &self,
        server: &Server,
        owner: RegionId,
        deadline: TickDeadline,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) {
        let Ok(published) = self.regional_actors.try_read() else {
            return;
        };
        let plan = published.clone();
        drop(published);
        let Some(actors) = plan.by_region.get(&owner) else {
            return;
        };
        let item_timers = ItemEntity::timers_unobserved(server);
        let native = server.plugin_manager.try_without_native_code();
        let pickups = native.is_some() && Self::regional_pickups_unobserved(server);
        let mut expired = Vec::new();
        let mut collected = FxHashSet::default();
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
            let Ok(published) = self.regional_actors.try_read() else {
                break;
            };
            if published.assigned.get(&base.entity_id) != Some(&owner) {
                continue;
            }
            let publish = |java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>| {
                if owns(base.chunk_pos.load()) {
                    self.entity_tracker.try_publish_regional_drop_move(
                        self,
                        server,
                        actor,
                        java_recipients,
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
                        if pickups
                            && self
                                .try_collect_regional_drop(
                                    server,
                                    actor,
                                    item,
                                    owns,
                                    &mut java_recipients,
                                )
                                .is_some()
                        {
                            collected.insert(base.entity_id);
                        } else {
                            item.try_regional_motion(self, chunk, owner, deadline, owns, || {
                                publish(&mut java_recipients);
                            });
                        }
                    }
                    RegionalItemStep::Expired => expired.push(actor),
                    _ => {}
                }
            } else if native.is_some()
                && let Some(orb) = actor_data.downcast_ref::<ExperienceOrbEntity>()
                && let Some(_checkpoint) = server.try_entity_save_guard()
            {
                orb.try_regional_orb_tick(self, chunk, owner, deadline, owns, || {
                    publish(&mut java_recipients);
                });
                if orb.has_expired() {
                    expired.push(actor);
                }
            } else if native.is_some()
                && base.regional_effects.load(Relaxed)
                && let Some(living) = actor.get_living_entity()
                && let Some(_checkpoint) = server.try_entity_save_guard()
                && let TryResult::Present(_loaded) = self.level.loaded_chunks.try_get(&chunk)
            {
                living.advance_regional_effects(
                    deadline,
                    || {
                        !base.is_removed()
                            && std::ptr::eq(base.world.load().as_ref(), self)
                            && base.chunk_pos.load() == chunk
                            && owns(chunk)
                    },
                    |expired, data| {
                        self.entity_tracker.try_publish_regional_effect_changes(
                            self,
                            server,
                            actor.as_ref(),
                            expired,
                            data,
                            &mut java_recipients,
                        )
                    },
                );
            }
        }
        EntityTracker::flush_regional_java_packets(java_recipients);
        self.finish_regional_drop_removals(server, collected);
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
        let Some(_checkpoint) = server.try_entity_save_guard() else {
            return;
        };
        let Ok(_snapshot) = self.entity_chunk_changes.snapshot.try_read() else {
            return;
        };
        let Ok(published) = self.regional_actors.try_read() else {
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
            let _ = base.try_with_unmounted_drop(|| {
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
                    pending.push(RegionalEntityUpdate::Removed(actor.clone()));
                }
            });
        }
        self.finish_regional_drop_removals(server, discarded);
    }

    fn finish_regional_drop_removals(&self, server: &Server, removed: FxHashSet<i32>) {
        if removed.is_empty() {
            return;
        }
        // One registry update for the batch. Viewers that require translation
        // or an observer remain paired for the world's queued cleanup.
        self.entities.rcu(|current| {
            current
                .iter()
                .filter(|actor| !removed.contains(&actor.get_entity().entity_id))
                .cloned()
                .collect::<Vec<_>>()
        });
        self.entity_tracker
            .try_publish_regional_drop_removals(self, server, removed);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        path::Path,
        time::{Duration, Instant},
    };

    use pumpkin_data::{
        effect::StatusEffect, entity::EntityType, item::Item, item_stack::ItemStack, potion::Effect,
    };
    use pumpkin_nbt::NbtCompound;
    use pumpkin_world::chunk::ChunkData;
    use sculith_config::{
        AdvancedConfiguration, BasicConfiguration, telemetry::TelemetryConfig, world::WorldPreset,
    };
    use sculith_util::math::vector3::Vector3;

    use crate::data::VanillaData;
    use crate::entity::living::RegionalActorDelivery;

    use super::*;

    async fn test_server(root: &Path) -> Arc<Server> {
        let basic = BasicConfiguration {
            default_level_name: root.join("world").to_string_lossy().into_owned(),
            allow_chat_reports: false,
            allow_nether: false,
            allow_end: false,
            ..Default::default()
        };
        let mut advanced = AdvancedConfiguration::default();
        advanced.world.generator = WorldPreset::Flat;
        advanced.networking.bedrock.online_mode = false;
        Server::new(
            basic,
            advanced,
            TelemetryConfig::default(),
            VanillaData::default(),
            Vec::new(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_handoff_in_another_region_preserves_the_running_item_step() {
        let folder = tempfile::tempdir().unwrap();
        let server = test_server(folder.path()).await;
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
        world.publish_region_layout(&layout, false);
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
        let previous = world.regional_actors.read().unwrap().clone();
        world.assign_regional_actors(
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
        world.advance_regional_actors(&server, 0, deadline, &|chunk| {
            // Place the handoff after the running job has read its actor list.
            if !moved.replace(true) {
                crossing.get_entity().set_pos(position(destination));
            }
            layout.owner(chunk) == Some(0)
        });
        let current = world.regional_actors.read().unwrap().clone();
        assert_eq!(current.assigned[&crossing.get_entity().entity_id], 2);
        let mut data = NbtCompound::new();
        local.write_nbt(&mut data);
        assert_eq!(
            data.get_short("PickupDelay"),
            Some(i16::from(ItemEntity::DEFAULT_PICKUP_DELAY - 1))
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn an_inactive_mob_resumes_effect_work_when_its_region_is_published() {
        let folder = tempfile::tempdir().unwrap();
        let server = test_server(folder.path()).await;
        let world = server.worlds.load()[0].clone();
        let west = Vector2::new(0, 0);
        let east = Vector2::new(10, 0);
        let layout = |east_owner| {
            let mut owners = FxHashMap::from_iter([(west, 0)]);
            let mut regions = FxHashMap::from_iter([(0, [west].into())]);
            if let Some(id) = east_owner {
                owners.insert(east, id);
                regions.insert(id, [east].into());
            }
            Arc::new(RegionLayout::from_active(owners, regions))
        };
        for chunk in [west, east] {
            world
                .level
                .loaded_chunks
                .insert(chunk, ChunkData::empty_sync(chunk.x, chunk.y));
        }
        let inactive = layout(None);
        world.publish_region_layout(&inactive, false);

        let pig = crate::entity::r#type::from_type(
            &EntityType::PIG,
            Vector3::new(168.0, 80.0, 8.0),
            &world,
            uuid::Uuid::new_v4(),
        );
        let id = pig.get_entity().entity_id;
        let living = pig.get_living_entity().unwrap();
        world.add_entity_silent(pig.clone());
        let vision = &StatusEffect::NIGHT_VISION;
        living.add_effect(Effect {
            effect_type: vision,
            duration: 4,
            amplifier: 0,
            ambient: false,
            show_particles: false,
            show_icon: false,
            blend: false,
        });
        let remaining = || living.get_effect(vision).unwrap().duration;
        assert!(!world.regional_actors.read().unwrap().contains(id));

        let at = Instant::now();
        let period = Duration::from_millis(50);
        let step = |layout: &RegionLayout, owner, ticks| {
            world.advance_region_time(
                owner,
                layout.chunks(owner).unwrap(),
                TickDeadline {
                    at: at + period * ticks,
                    period,
                },
            );
        };
        let first = layout(Some(1));
        world.publish_region_layout(&first, false);
        step(&first, 1, 0);
        assert_eq!(remaining(), 3);
        world.publish_region_layout(&inactive, false);
        step(&first, 1, 1);
        assert_eq!(remaining(), 3);
        assert!(!world.regional_actors.read().unwrap().contains(id));

        let next = layout(Some(2));
        world.publish_region_layout(&next, false);
        step(&next, 2, 0);
        assert_eq!(remaining(), 3);
        step(&next, 2, 2);
        assert_eq!(remaining(), 2);
        assert_eq!(world.regional_actors.read().unwrap().assigned[&id], 2);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_mob_keeps_its_effect_deadline_and_pending_work_across_regions() {
        let folder = tempfile::tempdir().unwrap();
        let server = test_server(folder.path()).await;
        let world = server.worlds.load()[0].clone();
        let west = Vector2::new(0, 0);
        let east = Vector2::new(8, 0);
        let layout = Arc::new(RegionLayout::from_active(
            FxHashMap::from_iter([(west, 0), (east, 1)]),
            FxHashMap::from_iter([(0, [west].into()), (1, [east].into())]),
        ));
        world.publish_region_layout(&layout, false);
        for chunk in [west, east] {
            world
                .level
                .loaded_chunks
                .insert(chunk, ChunkData::empty_sync(chunk.x, chunk.y));
        }

        let pig = crate::entity::r#type::from_type(
            &EntityType::PIG,
            Vector3::new(8.0, 80.0, 8.0),
            &world,
            uuid::Uuid::new_v4(),
        );
        let base = pig.get_entity();
        base.has_no_gravity.store(true, Relaxed);
        pig.get_mob().unwrap().get_mob_entity().set_no_ai(true);
        world.add_entity_silent(pig.clone());
        let previous = world.regional_actors.read().unwrap().clone();
        world.assign_regional_actors(&server, &[(&pig, west)], true, &previous);
        let living = pig.get_living_entity().unwrap();
        let vision = &StatusEffect::NIGHT_VISION;
        let add = || {
            living.add_effect(Effect {
                effect_type: vision,
                duration: 2,
                amplifier: 0,
                ambient: false,
                show_particles: true,
                show_icon: true,
                blend: false,
            });
        };
        let remaining = || living.get_effect(vision).map(|effect| effect.duration);
        let first = TickDeadline {
            at: Instant::now(),
            period: Duration::from_millis(50),
        };
        let next = TickDeadline {
            at: first.at + first.period,
            ..first
        };
        let step = |owner, deadline| {
            world.advance_region_time(owner, layout.chunks(owner).unwrap(), deadline);
        };
        add();
        step(0, first);
        assert_eq!(remaining(), Some(1));
        base.set_pos(Vector3::new(136.0, 80.0, 8.0));
        assert_eq!(
            world.regional_actors.read().unwrap().assigned[&base.entity_id],
            1
        );
        step(0, next);
        step(1, first);
        assert_eq!(remaining(), Some(1));
        step(1, next);
        tick_actor_with_deadline(base.entity_id, Some(next), || {
            pig.tick(pig.as_ref(), &server);
        });
        step(1, next);
        assert_eq!(remaining(), Some(0));

        let later = TickDeadline {
            at: next.at + next.period,
            ..next
        };
        step(1, later);
        assert_eq!(remaining(), None);

        let stale = world.regional_actors.read().unwrap().clone();
        add();
        let plan = world.assign_regional_actors(&server, &[], true, &stale);
        assert_eq!(plan.assigned[&base.entity_id], 1);
        let deadline = |ticks| TickDeadline {
            at: later.at + later.period * ticks,
            ..later
        };
        step(1, deadline(1));
        assert_eq!(remaining(), Some(1));
        step(1, deadline(2));
        assert_eq!(remaining(), Some(0));
        assert!(living.advance_regional_effects(deadline(3), || true, |_, _| None));
        assert_eq!(remaining(), Some(0));
        tick_actor_with_deadline(base.entity_id, Some(deadline(3)), || {
            pig.tick(pig.as_ref(), &server);
        });
        assert_eq!(remaining(), None);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn regional_regeneration_and_expiry_commit_once_on_the_same_deadline() {
        let folder = tempfile::tempdir().unwrap();
        let server = test_server(folder.path()).await;
        let world = server.worlds.load()[0].clone();
        let pig = crate::entity::r#type::from_type(
            &EntityType::PIG,
            Vector3::new(8.0, 80.0, 8.0),
            &world,
            uuid::Uuid::new_v4(),
        );
        let living = pig.get_living_entity().unwrap();
        let max_health = living.get_max_health();
        let previous = max_health - 0.5;
        living.set_health(previous);
        living.add_effect(Effect {
            effect_type: &StatusEffect::REGENERATION,
            duration: 12,
            amplifier: 2,
            ambient: false,
            show_particles: false,
            show_icon: false,
            blend: false,
        });
        living.add_effect(Effect {
            effect_type: &StatusEffect::NIGHT_VISION,
            duration: 0,
            amplifier: 0,
            ambient: false,
            show_particles: true,
            show_icon: true,
            blend: false,
        });
        let deadline = TickDeadline {
            at: Instant::now(),
            period: Duration::from_millis(50),
        };

        assert!(living.advance_regional_effects(
            deadline,
            || true,
            |expired, batch| {
                assert_eq!(expired, [&StatusEffect::NIGHT_VISION]);
                assert_eq!(batch.health.unwrap().health, max_health);
                assert_eq!(batch.bedrock_particles, Some(0));
                assert!(batch.java.is_some());
                None
            }
        ));
        assert_eq!(living.health.load(), previous);
        assert!(living.has_effect(&StatusEffect::NIGHT_VISION));
        assert_eq!(
            living
                .get_effect(&StatusEffect::REGENERATION)
                .unwrap()
                .duration,
            12
        );

        assert!(living.advance_regional_effects(
            deadline,
            || true,
            |_, _| Some(RegionalActorDelivery::DELIVERED)
        ));
        assert_eq!(living.health.load(), max_health);
        assert!(!living.has_effect(&StatusEffect::NIGHT_VISION));
        assert_eq!(
            living
                .get_effect(&StatusEffect::REGENERATION)
                .unwrap()
                .duration,
            11
        );
        assert!(living.advance_regional_effects(
            deadline,
            || true,
            |_, _| { panic!("the same deadline was published twice") }
        ));
        assert_eq!(
            living
                .get_effect(&StatusEffect::REGENERATION)
                .unwrap()
                .duration,
            11
        );
        server.shutdown().await;
    }
}
