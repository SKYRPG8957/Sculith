use super::{
    NBTStorage, NBTStorageInit,
    living::{LivingEntity, RegionalEffectData, RegionalEffectHealth},
    player::Player,
};
use crossbeam::atomic::AtomicCell;
use pumpkin_data::attributes::Attributes;
use pumpkin_data::damage::DamageType;
use pumpkin_data::tracked_data::living_entity::DATA_HEALTH_ID;
use pumpkin_nbt::compound::NbtCompound;
use sculith_util::Difficulty;

use crate::{net::bedrock::BedrockClient, server::TickDeadline};

const MAX_FOOD: u8 = 20;
const EXHAUSTION_COST: f32 = 4.0;
const MAX_EXHAUSTION: f32 = 40.0;
const FOOD_HEAL_TICKS: u32 = 80;
const FOOD_HEAL_EXHAUSTION: f32 = 6.0;

pub struct HungerManager {
    pub level: AtomicCell<u8>,
    pub saturation: AtomicCell<f32>,
    pub exhaustion: AtomicCell<f32>,
    pub tick_timer: AtomicCell<u32>,
}

impl Default for HungerManager {
    fn default() -> Self {
        Self {
            level: AtomicCell::new(MAX_FOOD),
            saturation: AtomicCell::new(5.0),
            exhaustion: AtomicCell::new(0.0),
            tick_timer: AtomicCell::new(0),
        }
    }
}

impl HungerManager {
    pub fn tick(&self, player: &Player) {
        self.tick_with_deadline(player, None);
    }

    pub(super) fn tick_with_deadline(&self, player: &Player, deadline: Option<TickDeadline>) {
        let level_info = player.world().level_info.load();
        let difficulty = level_info.difficulty;
        let natural_regen = level_info.game_rules.natural_health_regeneration;
        let client = player
            .client
            .bedrock()
            .filter(|client| client.is_current_protocol());
        let effect_edit = client.map(|_| player.living_entity.hold_effect_edit());
        let health = player.living_entity.health.load();
        let can_heal = player.can_food_heal();

        let abilities = player.lock_abilities_and_hunger();
        let mut level = self.level.load();
        let mut saturation = self.saturation.load();
        let mut exhaustion = self.exhaustion.load();
        let mut timer = self.tick_timer.load();

        let mut needs_sync = false;
        let mut heal_amount = 0.0;
        let mut damage_amount = 0.0;

        let has_excess = exhaustion > EXHAUSTION_COST;
        let owns_drain = (has_excess || deadline.is_none())
            && client.is_none_or(|client| client.claim_food_drain(deadline));
        if has_excess && owns_drain {
            Self::spend_exhaustion(&mut level, &mut saturation, &mut exhaustion, difficulty);
            needs_sync = true;
        }

        let regenerating = natural_regen && level >= 18 && can_heal;
        let owns_regeneration = (regenerating || deadline.is_none())
            && client.is_none_or(|client| client.claim_food_regeneration(deadline));
        if regenerating {
            if owns_regeneration && Self::advance_regeneration_timer(&mut timer) {
                heal_amount = 1.0;
                exhaustion = (exhaustion + FOOD_HEAL_EXHAUSTION).min(MAX_EXHAUSTION);
                // Bedrock spends the healing cost in the same tick. Respect the
                // existing drain deadline if either owner spent exhaustion already.
                if !has_excess
                    && exhaustion > EXHAUSTION_COST
                    && (owns_drain || client.is_none_or(|client| client.claim_food_drain(deadline)))
                {
                    Self::spend_exhaustion(
                        &mut level,
                        &mut saturation,
                        &mut exhaustion,
                        difficulty,
                    );
                }
                needs_sync = true;
            }
        } else if level == 0 {
            timer += 1;
            if timer >= 80 {
                timer = 0;
                let should_starve = health > 10.0
                    || difficulty == Difficulty::Hard
                    || (health > 1.0 && difficulty == Difficulty::Normal);

                if should_starve {
                    damage_amount = 1.0;
                }
                self.tick_timer.store(0);
            }
        } else {
            timer = 0;
        }

        if needs_sync || timer != self.tick_timer.load() {
            self.level.store(level);
            self.saturation.store(saturation);
            self.exhaustion.store(exhaustion);
            self.tick_timer.store(timer);
        }
        drop(abilities);
        drop(effect_edit);

        if heal_amount > 0.0 {
            player.heal(heal_amount);
        } else if needs_sync {
            player.send_health();
        }
        if damage_amount > 0.0 {
            player
                .living_entity
                .damage(player, damage_amount, DamageType::STARVE);
        }
        if deadline.is_some() {
            self.offer_regional_processing(
                player,
                regenerating && difficulty != Difficulty::Peaceful,
            );
        }
    }

    const fn advance_regeneration_timer(timer: &mut u32) -> bool {
        *timer = timer.saturating_add(1);
        if *timer < FOOD_HEAL_TICKS {
            return false;
        }
        *timer = 0;
        true
    }

    fn spend_exhaustion(
        level: &mut u8,
        saturation: &mut f32,
        exhaustion: &mut f32,
        difficulty: Difficulty,
    ) {
        *exhaustion -= EXHAUSTION_COST;
        if *saturation > 0.0 {
            *saturation = (*saturation - 1.0).max(0.0);
        } else if difficulty != Difficulty::Peaceful {
            *level = level.saturating_sub(1);
        }
    }

    pub(crate) fn has_exhaustion_to_drain(&self) -> bool {
        self.exhaustion.load() > EXHAUSTION_COST
    }

    pub(super) fn offer_regional_drain(&self, player: &Player) {
        self.offer_regional_processing(player, false);
    }

    fn offer_regional_processing(&self, player: &Player, regeneration: bool) {
        let drain = self.has_exhaustion_to_drain();
        if !drain && !regeneration {
            return;
        }
        let Some(client) = player
            .client
            .bedrock()
            .filter(|client| client.is_current_protocol())
        else {
            return;
        };
        let world = player.world();
        if let Some(registered) = world.get_player_by_uuid(player.gameprofile.id)
            && std::ptr::eq(registered.as_ref(), player)
        {
            world.offer_regional_food_work(client.id, &registered, drain, regeneration);
        }
    }

    pub(crate) fn regenerate_on_region(
        &self,
        player: &Player,
        client: &BedrockClient,
        deadline: TickDeadline,
        owns: impl FnOnce() -> bool,
        publish: impl FnOnce(RegionalEffectData) -> Option<bool>,
    ) -> bool {
        let info = player.world().level_info.load_full();
        if info.difficulty == Difficulty::Peaceful || !info.game_rules.natural_health_regeneration {
            return false;
        }
        let living = &player.living_entity;
        let Ok(effects) = living.active_effects.try_lock() else {
            return true;
        };
        if !effects.is_empty() {
            return false;
        }
        let Some(_edit) = living.try_hold_effect_edit() else {
            return true;
        };
        let Ok(attributes) = living.attributes.try_read() else {
            return true;
        };
        let Ok(abilities) = player.abilities.try_lock() else {
            return true;
        };
        let health = living.health.load();
        let max_health =
            LivingEntity::attribute_value_from(&attributes, &Attributes::MAX_HEALTH) as f32;
        let mut level = self.level.load();
        let mut saturation = self.saturation.load();
        let mut exhaustion = self.exhaustion.load();
        if abilities.invulnerable
            || player.is_spectator()
            || living.is_dead_or_dying()
            || !(health > 0.0 && health < max_health && level >= 18)
            || !(0.0..=EXHAUSTION_COST).contains(&exhaustion)
        {
            return false;
        }
        if !owns() {
            return true;
        }
        let mut timer = self.tick_timer.load();
        let heal = Self::advance_regeneration_timer(&mut timer);
        let mut next_health = health;
        let committed = client.commit_regional_regeneration(deadline, heal, || {
            if heal {
                next_health = (health + 1.0).min(max_health);
                exhaustion = (exhaustion + FOOD_HEAL_EXHAUSTION).min(MAX_EXHAUSTION);
                Self::spend_exhaustion(&mut level, &mut saturation, &mut exhaustion, info.difficulty);
                if !living.entity.synched_data.try_publish_value(DATA_HEALTH_ID, next_health, |java| {
                    publish(RegionalEffectData {
                        java,
                        bedrock_particles: None,
                        health: Some(RegionalEffectHealth {
                            health: next_health,
                            max_health,
                            food: Some((level, saturation)),
                        }),
                    })
                }) {
                    return false;
                }
                living.health.store(next_health);
                self.level.store(level);
                self.saturation.store(saturation);
                self.exhaustion.store(exhaustion);
                tracing::trace!(player = %player.gameprofile.name, health = next_health, food = level,
                    saturation, exhaustion, "Applied natural regeneration on the player region");
            }
            self.tick_timer.store(timer);
            true
        });
        !committed || next_health < max_health && level >= 18
    }

    /// Consume the same exhaustion step as the world while the region owns the player.
    /// The world still advances regeneration and starvation and publishes health.
    pub(crate) fn drain_on_region(
        &self,
        player: &Player,
        client: &BedrockClient,
        deadline: TickDeadline,
        owns: impl FnOnce() -> bool,
    ) -> bool {
        if !client.is_current_protocol() {
            return false;
        }
        let Ok(abilities) = player.abilities.try_lock() else {
            return true;
        };
        let mut exhaustion = self.exhaustion.load();
        let mut level = self.level.load();
        let mut saturation = self.saturation.load();
        let health = player.living_entity.health.load();
        let info = player.world().level_info.load_full();
        if exhaustion <= EXHAUSTION_COST
            || abilities.invulnerable
            || player.is_spectator()
            || !player.has_client_loaded()
            || !player.has_stable_world_membership()
            || !(health > 0.0 && level > 1)
            || info.difficulty == Difficulty::Peaceful
            || self.tick_timer.load() != 0
        {
            return false;
        }
        if info.game_rules.natural_health_regeneration && level >= 18 {
            let Some(max_health) = player.living_entity.try_max_health() else {
                return true;
            };
            if health < max_health {
                return false;
            }
        }
        if !owns() || !client.claim_food_drain(Some(deadline)) {
            return true;
        }
        Self::spend_exhaustion(
            &mut level,
            &mut saturation,
            &mut exhaustion,
            info.difficulty,
        );
        self.level.store(level);
        self.saturation.store(saturation);
        self.exhaustion.store(exhaustion);
        client.mark_regional_food_dirty();
        tracing::trace!(player = %player.gameprofile.name, food = level, saturation, exhaustion, "Applied exhaustion on the player region");
        exhaustion > EXHAUSTION_COST
    }

    pub fn eat(&self, player: &Player, food: u8, saturation: f32) {
        let abilities = player.lock_abilities_and_hunger();
        let (level, saturation) = self.meal_values(food, saturation);
        self.level.store(level);
        self.saturation.store(saturation);
        drop(abilities);

        player.send_health();
    }

    pub(super) fn meal_values(&self, food: u8, saturation: f32) -> (u8, f32) {
        let level = self.level.load().saturating_add(food).min(MAX_FOOD);
        let saturation = (self.saturation.load() + saturation).clamp(0.0, f32::from(level));
        (level, saturation)
    }

    /// Add exhaustion to trigger hunger decrease
    pub fn add_exhaustion(&self, exhaustion: f32) {
        let current = self.exhaustion.load();
        self.exhaustion
            .store((current + exhaustion).min(MAX_EXHAUSTION));
    }

    /// Add hunger manually
    pub fn add_hunger(&self, hunger: u8) {
        let current = self.level.load();
        self.level.store((current + hunger).min(MAX_FOOD));
    }

    /// Add saturation manually
    pub fn add_saturation(&self, saturation: f32) {
        let current = self.saturation.load();
        self.saturation
            .store((current + saturation).min(f32::from(self.level.load())));
    }

    pub fn set_level(&self, level: u8) {
        self.level.store(level);
    }

    pub fn set_saturation(&self, saturation: f32) {
        self.saturation.store(saturation);
    }

    pub fn get_exhaustion(&self) -> f32 {
        self.exhaustion.load()
    }

    pub fn set_exhaustion(&self, exhaustion: f32) {
        self.exhaustion.store(exhaustion.min(MAX_EXHAUSTION));
    }

    pub fn restart(&self) {
        self.level.store(MAX_FOOD);
        self.saturation.store(5.0);
        self.exhaustion.store(0.0);
        self.tick_timer.store(0);
    }
}

impl NBTStorage for HungerManager {
    fn write_nbt(&self, nbt: &mut NbtCompound) {
        nbt.put_int("foodLevel", self.level.load().into());
        nbt.put_float("foodSaturationLevel", self.saturation.load());
        nbt.put_float("foodExhaustionLevel", self.exhaustion.load());
        nbt.put_int("foodTickTimer", self.tick_timer.load() as i32);
    }

    fn read_nbt_non_mut(&self, nbt: &NbtCompound) {
        self.level
            .store(nbt.get_int("foodLevel").unwrap_or(20) as u8);
        self.saturation
            .store(nbt.get_float("foodSaturationLevel").unwrap_or(5.0));
        self.exhaustion
            .store(nbt.get_float("foodExhaustionLevel").unwrap_or(0.0));
        self.tick_timer
            .store(nbt.get_int("foodTickTimer").unwrap_or(0) as u32);
    }
}

impl NBTStorageInit for HungerManager {}
