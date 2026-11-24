use pumpkin_data::tracked_data;
use std::{
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicI32, Ordering::Relaxed},
    },
    time::Instant,
};

use crate::{entity::mob::Mob, server::TickDeadline};

pub const BABY_START_AGE: i32 = -24000;
pub const DEFAULT_BABY_SPAWN_CHANCE: f32 = 0.05;
pub const ABSTRACT_HORSE_BABY_SPAWN_CHANCE: f32 = 0.2;
pub const OCELOT_BABY_SPAWN_CHANCE: f32 = 1.0;
pub const PANDA_BABY_SPAWN_CHANCE: f32 = 0.2;
pub const POLAR_BEAR_BABY_SPAWN_CHANCE: f32 = 1.0;
pub const STRIDER_BABY_SPAWN_CHANCE: f32 = 0.5;
pub const FORCED_AGE_PARTICLE_TICKS: i32 = 40;
pub const AGE_LOCK_COOLDOWN_TICKS: i32 = 40;

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

fn publish_age_boundary(mob: &(impl AgeableMob + ?Sized), before: i32, after: i32) {
    if (before < 0) == (after < 0) {
        return;
    }
    let entity = mob.get_entity();
    entity.set_synced_data(tracked_data::ageable_mob::DATA_BABY_ID, after < 0);
    entity.refresh_dimensions();
    mob.age_boundary_reached();
    let mut actor_data = entity.bedrock_metadata();
    if let Some(meta) = mob.mob_metadata() {
        actor_data.merge(meta.to_bedrock_actor_data());
    }
    entity.send_bedrock_actor_data(&actor_data);
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
