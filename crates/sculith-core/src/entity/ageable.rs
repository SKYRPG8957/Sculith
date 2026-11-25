use pumpkin_data::{Block, data_component_impl::EquipmentSlot, entity::EntityType, tracked_data};
use rustc_hash::FxHashMap;
use sculith_protocol::bedrock::client::set_actor_data::{
    SyncedActorDataList, entity_data_flag, entity_data_key,
};
use sculith_util::math::{
    boundingbox::{BoundingBox, EntityDimensions},
    position::BlockPos,
    vector2::Vector2,
    vector3::Vector3,
};
use std::{
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicI32, Ordering::Relaxed},
    },
    time::Instant,
};

use crate::{
    entity::mob::{Mob, MobMetadata},
    net::ClientPlatform,
    server::TickDeadline,
    world::{COLLISION_LOOK_BELOW, World},
};

use super::{Entity, EntityBase};

pub const BABY_START_AGE: i32 = -24000;
pub const DEFAULT_BABY_SPAWN_CHANCE: f32 = 0.05;
pub const ABSTRACT_HORSE_BABY_SPAWN_CHANCE: f32 = 0.2;
pub const OCELOT_BABY_SPAWN_CHANCE: f32 = 1.0;
pub const PANDA_BABY_SPAWN_CHANCE: f32 = 0.2;
pub const POLAR_BEAR_BABY_SPAWN_CHANCE: f32 = 1.0;
pub const STRIDER_BABY_SPAWN_CHANCE: f32 = 0.5;
pub const FORCED_AGE_PARTICLE_TICKS: i32 = 40;
pub const AGE_LOCK_COOLDOWN_TICKS: i32 = 40;

pub(super) fn supports_regional_maturation(kind: &EntityType) -> bool {
    [
        &EntityType::PIG,
        &EntityType::COW,
        &EntityType::SHEEP,
        &EntityType::CHICKEN,
    ]
    .contains(&kind)
}

pub struct AgeableData {
    pub forced_age: AtomicI32,
    pub forced_age_timer: AtomicI32,
    pub age_locked: AtomicBool,
    pub age_lock_particle_timer: AtomicI32,
    last_growth_step: Mutex<Option<Instant>>,
}

impl Default for AgeableData {
    fn default() -> Self {
        Self {
            forced_age: AtomicI32::new(0),
            forced_age_timer: AtomicI32::new(0),
            age_locked: AtomicBool::new(false),
            age_lock_particle_timer: AtomicI32::new(0),
            last_growth_step: Mutex::default(),
        }
    }
}

impl AgeableData {
    fn lock_growth(&self) -> MutexGuard<'_, Option<Instant>> {
        self.last_growth_step
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn start_restored_growth(&self, deadline: TickDeadline) {
        *self.lock_growth() = Some(deadline.at);
    }
}

fn advance_age(
    mob: &(impl AgeableMob + ?Sized),
    last_step: &mut Option<Instant>,
    deadline: Option<TickDeadline>,
    can_mature: bool,
) -> Option<(i32, i32)> {
    if let Some((deadline, previous)) = deadline.zip(*last_step)
        && !deadline.follows(previous)
    {
        return None;
    }
    let entity = mob.get_entity();
    let before = mob.get_age();
    let after = if before < 0 && mob.can_age_up() {
        before + 1
    } else if before > 0 {
        before - 1
    } else {
        before
    };
    // Maturation can change dimensions, run species hooks and publish metadata.
    if !can_mature && (before == after || (before < 0 && after == 0)) {
        return None;
    }
    if before != after {
        entity
            .age
            .compare_exchange(before, after, Relaxed, Relaxed)
            .ok()?;
    }
    *last_step = deadline.map(|step| step.at);
    (before != after).then_some((before, after))
}

pub(crate) fn advance_regional_growth(
    mob: &dyn AgeableMob,
    deadline: TickDeadline,
    still_owned: impl FnOnce() -> bool,
) -> Option<i32> {
    if !mob.can_be_a_baby() {
        return None;
    }
    let mut last_step = mob.get_ageable_data().last_growth_step.try_lock().ok()?;
    if !still_owned() {
        return None;
    }
    advance_age(mob, &mut last_step, Some(deadline), false).map(|(_, after)| after)
}

/// Returns whether all paired Bedrock viewers received the transition when applied.
pub(crate) fn try_mature_on_region(
    actor: &dyn EntityBase,
    mob: &dyn AgeableMob,
    world: &World,
    deadline: TickDeadline,
    owns: &impl Fn(Vector2<i32>) -> bool,
    java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
) -> Option<bool> {
    let entity = actor.get_entity();
    if !supports_regional_maturation(entity.entity_type) || mob.get_age() != -1 {
        return None;
    }
    entity.try_with_unmounted_actor(|| {
        let living = &mob.get_mob_entity().living_entity;
        let equipment = living.entity_equipment.try_lock().ok()?;
        if !equipment.get(&EquipmentSlot::SADDLE).is_empty()
            || entity.leashed_to.try_lock().ok()?.is_some()
            || !living.is_alive()
            || living.is_dead_or_dying()
        {
            return None;
        }
        let mut step = mob.get_ageable_data().last_growth_step.try_lock().ok()?;
        if mob.get_age() != -1 || !mob.can_age_up() || step.is_some_and(|at| !deadline.follows(at))
        {
            return None;
        }
        let position = entity.pos.load();
        let before = entity.entity_dimension.load();
        let adult = entity.dimensions_for_age(entity.pose.load(), false);
        let new_bounds = Entity::make_bounding_box(entity.entity_type, position, &adult);
        let area = new_bounds.minmax(&entity.bounding_box.load());
        let mut data = maturation_actor_data(entity.bedrock_metadata_for_size(adult, false), mob);
        omit_unchanged_bedrock_bounds(entity, &mut data, (before, true), (adult, false));
        let mut bedrock_delivered = None;
        let apply = || {
            entity.synched_data.try_publish_value(
                tracked_data::ageable_mob::DATA_BABY_ID,
                false,
                |java| {
                    let delivered = world.entity_tracker.try_publish_regional_maturation(
                        world,
                        actor,
                        data,
                        java,
                        java_recipients,
                        || {
                            if entity.is_removed()
                                || !std::ptr::eq(entity.world.load().as_ref(), world)
                                || !owns(entity.chunk_pos.load())
                            {
                                return None;
                            }
                            entity.age.store(0, Relaxed);
                            entity.entity_dimension.store(adult);
                            entity.bounding_box.value.store(new_bounds);
                            *step = Some(deadline.at);
                            Some(())
                        },
                    )?;
                    bedrock_delivered = Some(delivered.bedrock_queued);
                    Some(delivered.java_queued)
                },
            )
        };
        let resize = || {
            world.entity_index.try_update_isolated_actor_bounds(
                world,
                entity.entity_id,
                area,
                area,
                new_bounds,
                apply,
            );
        };
        entity.bounding_box.try_with_current_world(world, || {
            let check_room = !entity.first_tick.load(Relaxed)
                && !entity.no_physics.load(Relaxed)
                && (adult.width > before.width || adult.height > before.height);
            if check_room {
                let (center, allowed) = Entity::growth_centers(position, before, adult);
                let size = Vector3::new(
                    f64::from(adult.width),
                    f64::from(adult.height),
                    f64::from(adult.width),
                );
                fits_at_original_position(world, position, center, &allowed, size, owns, resize);
            } else {
                resize();
            }
        })?;
        bedrock_delivered
    })?
}

fn maturation_actor_data(
    mut metadata: SyncedActorDataList,
    mob: &dyn AgeableMob,
) -> SyncedActorDataList {
    let extra = if mob.get_entity().entity_type == &EntityType::SHEEP {
        // Preserve the wool from the pre-growth snapshot and publish the adult scale.
        mob.mob_metadata().map(|metadata| metadata.scale(1.0))
    } else {
        mob.get_mob_entity()
            .is_no_ai()
            .then(|| MobMetadata::new().no_ai(true))
    };
    if let Some(extra) = extra {
        metadata.merge(extra.to_bedrock_actor_data());
    }
    metadata.set_actor_flag(entity_data_flag::BABY, false);
    metadata
}

fn fits_at_original_position(
    world: &World,
    position: Vector3<f64>,
    center: Vector3<f64>,
    allowed: &BoundingBox,
    size: Vector3<f64>,
    owns: &impl Fn(Vector2<i32>) -> bool,
    apply: impl FnOnce(),
) {
    if !(0.0..=4.0).contains(&size.x) || !(0.0..=4.0).contains(&size.y) {
        return;
    }
    let envelope = allowed.expand(size.x, size.y, size.z);
    let min = BlockPos::floored_v(envelope.min.add_raw(0.0, -COLLISION_LOOK_BELOW, 0.0));
    let max = envelope.max_block_pos();
    world.level.try_with_regional_path(
        min,
        max,
        owns,
        |read| {
            let mut collisions = Vec::new();
            for pos in BlockPos::iterate(min, max) {
                let state = read(pos)?.to_state();
                if state.is_air() {
                    continue;
                }
                if Block::from_state_id(state.id) == &Block::POWDER_SNOW {
                    return None;
                }
                collisions.extend(
                    state
                        .get_block_collision_shapes_at(&pos)
                        .map(|shape| shape.at_pos(pos))
                        .filter(|shape| shape.intersects(&envelope)),
                );
            }
            let border = world.worldborder.try_lock().ok()?;
            let adjusted = World::nearest_free_center(allowed, center, size, collisions, &border)?;
            (adjusted.add_raw(0.0, -size.y / 2.0, 0.0) == position)
                .then_some((BlockPos::floored_v(position), border))
        },
        |border, _| {
            // Hold the border and sampled palettes through the state change.
            apply();
            drop(border);
        },
    );
}

pub(super) fn publish_age_boundary(mob: &(impl AgeableMob + ?Sized), before: i32, after: i32) {
    if (before < 0) == (after < 0) {
        return;
    }
    let entity = mob.get_entity();
    entity.set_synced_data(tracked_data::ageable_mob::DATA_BABY_ID, after < 0);
    let previous = if entity.entity_type.bedrock_age_geometry().is_some() {
        let previous = entity.entity_dimension.load();
        entity.refresh_dimensions_into(None);
        Some((previous, before < 0))
    } else {
        entity.refresh_dimensions();
        None
    };
    mob.age_boundary_reached();
    publish_bedrock_age(mob, previous);
}

pub(crate) fn publish_bedrock_age(
    mob: &(impl Mob + ?Sized),
    previous: Option<(EntityDimensions, bool)>,
) {
    let entity = mob.get_entity();
    let _motion = entity.hold_actor_motion();
    let current = (entity.entity_dimension.load(), entity.age.load(Relaxed) < 0);
    let mut actor_data = entity.bedrock_metadata_for_size(current.0, current.1);
    if let Some(meta) = mob.mob_metadata() {
        actor_data.merge(meta.to_bedrock_actor_data());
    }
    if let Some(previous) = previous {
        omit_unchanged_bedrock_bounds(entity, &mut actor_data, previous, current);
    }
    entity.send_bedrock_actor_data(&actor_data);
}

fn omit_unchanged_bedrock_bounds(
    entity: &Entity,
    metadata: &mut SyncedActorDataList,
    previous: (EntityDimensions, bool),
    current: (EntityDimensions, bool),
) {
    if entity.entity_type.bedrock_age_geometry().is_some()
        && entity.bedrock_packet_dimensions(previous.0, previous.1)
            == entity.bedrock_packet_dimensions(current.0, current.1)
    {
        metadata.0.remove(&entity_data_key::WIDTH);
        metadata.0.remove(&entity_data_key::HEIGHT);
    }
}

#[must_use]
pub fn get_speed_up_seconds_when_feeding(ticks_until_adult: i32) -> i32 {
    ((ticks_until_adult / 20) as f32 * 0.1) as i32
}

#[must_use]
pub fn can_use_golden_dandelion(
    stack: &pumpkin_data::item_stack::ItemStack,
    is_baby: bool,
    cooldown: i32,
    mob: &(impl Mob + ?Sized),
) -> bool {
    use pumpkin_data::{
        item::Item,
        tag::{self, Taggable},
    };
    stack.item == &Item::GOLDEN_DANDELION
        && is_baby
        && cooldown == 0
        && !mob
            .get_entity()
            .entity_type
            .has_tag(&tag::EntityType::MINECRAFT_CANNOT_BE_AGE_LOCKED)
}

pub fn set_age_locked(
    mob: &(impl Mob + ?Sized),
    is_age_locked: impl Fn() -> bool,
    player: &crate::entity::player::Player,
    stack: &mut pumpkin_data::item_stack::ItemStack,
    set_age_lock_data: impl FnOnce(),
) {
    use pumpkin_data::sound::{Sound, SoundCategory};
    let entity = mob.get_entity();
    set_age_lock_data();
    stack.decrement_unless_creative(player.gamemode.load(), 1);
    let age_locked = is_age_locked();
    if age_locked {
        mob.get_mob_entity()
            .persistence_required
            .store(true, Relaxed);
    }
    entity.world.load().play_sound(
        if age_locked {
            Sound::ItemGoldenDandelionUse
        } else {
            Sound::ItemGoldenDandelionUnuse
        },
        SoundCategory::Players,
        &entity.block_pos.load().0.to_f64().add_raw(0.5, 0.5, 0.5),
    );
}

pub trait AgeableMob: Mob {
    fn get_ageable_data(&self) -> &AgeableData;

    fn get_baby_start_age(&self) -> i32 {
        BABY_START_AGE
    }

    fn is_baby(&self) -> bool {
        self.can_be_a_baby() && self.get_mob_entity().living_entity.is_baby()
    }

    fn set_baby(&self, baby: bool) {
        if self.can_be_a_baby() {
            self.set_age(if baby { self.get_baby_start_age() } else { 0 });
        }
    }

    fn get_age(&self) -> i32 {
        self.get_mob_entity().living_entity.entity.age.load(Relaxed)
    }

    fn set_age(&self, new_age: i32) {
        let _motion = self.get_entity().hold_actor_motion();
        let old_age = {
            let _growth = self.get_ageable_data().lock_growth();
            self.get_entity().age.swap(new_age, Relaxed)
        };
        publish_age_boundary(self, old_age, new_age);
    }

    fn age_boundary_reached(&self) {}

    fn default_should_spawn_baby(&self) -> bool {
        true
    }

    fn default_baby_spawn_chance(&self) -> f32 {
        DEFAULT_BABY_SPAWN_CHANCE
    }

    fn finalize_ageable_spawn(
        &self,
        group_data: Option<crate::entity::mob::spawn::SpawnGroupData>,
    ) -> Option<crate::entity::mob::spawn::SpawnGroupData> {
        use crate::entity::mob::spawn::SpawnGroupData;
        use rand::RngExt;

        let (group_size, should_spawn_baby, baby_spawn_chance) = match group_data {
            Some(SpawnGroupData::Ageable {
                group_size,
                should_spawn_baby,
                baby_spawn_chance,
            }) => (group_size, should_spawn_baby, baby_spawn_chance),
            _ => (
                0,
                self.default_should_spawn_baby(),
                self.default_baby_spawn_chance(),
            ),
        };
        if should_spawn_baby
            && group_size > 0
            && self.get_random().random::<f32>() <= baby_spawn_chance
        {
            self.set_baby(true);
        }
        self.get_mob_entity().finalize_spawn_base();
        Some(SpawnGroupData::Ageable {
            group_size: group_size + 1,
            should_spawn_baby,
            baby_spawn_chance,
        })
    }

    fn is_age_locked(&self) -> bool {
        self.get_ageable_data().age_locked.load(Relaxed)
    }

    fn set_age_locked(&self, locked: bool) {
        let _motion = self.get_entity().hold_actor_motion();
        let data = self.get_ageable_data();
        let growth = data.lock_growth();
        data.age_locked.store(locked, Relaxed);
        drop(growth);
        self.get_entity()
            .set_synced_data(tracked_data::ageable_mob::AGE_LOCKED, locked);
    }

    fn ageable_interact(
        &self,
        player: &std::sync::Arc<crate::entity::player::Player>,
        stack: &mut pumpkin_data::item_stack::ItemStack,
    ) -> bool {
        if !can_use_golden_dandelion(
            stack,
            self.is_baby(),
            self.get_ageable_data()
                .age_lock_particle_timer
                .load(Relaxed),
            self,
        ) {
            return false;
        }
        set_age_locked(
            self,
            || self.is_age_locked(),
            player,
            stack,
            || {
                self.set_age_locked(!self.is_age_locked());
                self.set_age(self.get_baby_start_age());
                self.get_ageable_data()
                    .age_lock_particle_timer
                    .store(AGE_LOCK_COOLDOWN_TICKS, Relaxed);
            },
        );
        true
    }

    fn tick_age_lock_particle_timer(&self) {
        let timer = &self.get_ageable_data().age_lock_particle_timer;
        let ticks = timer.load(Relaxed);
        if ticks > 0 {
            timer.store(ticks - 1, Relaxed);
        }
    }

    fn can_age_up(&self) -> bool {
        self.is_baby() && !self.is_age_locked()
    }

    fn age_up(&self, seconds: i32, forced: bool) {
        let _motion = self.get_entity().hold_actor_motion();
        let data = self.get_ageable_data();
        let (old_age, age) = {
            let _growth = data.lock_growth();
            let old_age = self.get_age();
            let age = old_age.wrapping_add(seconds.wrapping_mul(20)).min(0);
            self.get_entity().age.store(age, Relaxed);
            (old_age, age)
        };
        publish_age_boundary(self, old_age, age);

        if forced {
            data.forced_age
                .fetch_add(age.wrapping_sub(old_age), Relaxed);
            if data.forced_age_timer.load(Relaxed) == 0 {
                data.forced_age_timer.store(40, Relaxed);
            }
        }

        if self.get_age() == 0 {
            self.set_age(data.forced_age.load(Relaxed));
        }
    }

    fn write_ageable_nbt(&self, nbt: &mut pumpkin_nbt::compound::NbtCompound) {
        if self.can_be_a_baby() {
            nbt.put_int("Age", self.get_age());
            nbt.put_int(
                "ForcedAge",
                self.get_ageable_data().forced_age.load(Relaxed),
            );
            nbt.put_bool("AgeLocked", self.is_age_locked());
        }
    }

    fn read_ageable_nbt(&self, nbt: &pumpkin_nbt::compound::NbtCompound) {
        if self.can_be_a_baby() {
            let _motion = self.get_entity().hold_actor_motion();
            let data = self.get_ageable_data();
            let age = nbt.get_int("Age").unwrap_or(0);
            let locked = nbt.get_bool("AgeLocked").unwrap_or(false);
            let old_age = {
                let _growth = data.lock_growth();
                data.forced_age
                    .store(nbt.get_int("ForcedAge").unwrap_or(0), Relaxed);
                data.age_locked.store(locked, Relaxed);
                self.get_entity().age.swap(age, Relaxed)
            };
            publish_age_boundary(self, old_age, age);
            self.get_entity()
                .set_synced_data(tracked_data::ageable_mob::AGE_LOCKED, locked);
        }
    }

    fn can_be_a_baby(&self) -> bool {
        true
    }

    fn ageable_ai_step(&self) {
        self.tick_age_lock_particle_timer();
        let deadline = crate::world::actor_deadline(self.get_entity().entity_id);
        let advanced = {
            let mut last_step = self.get_ageable_data().lock_growth();
            advance_age(self, &mut last_step, deadline, true)
        };
        if let Some((before, after)) = advanced {
            publish_age_boundary(self, before, after);
        }
    }
}
