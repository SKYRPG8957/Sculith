use crate::entity::EntityBase;
use crate::entity::living::RegionalActorDelivery;
use crate::entity::player::Player;
use crate::server::TickDeadline;
use crate::world::World;
use pumpkin_data::block_properties::is_air;
use pumpkin_data::damage::DamageType;
use pumpkin_data::effect::StatusEffect;
use pumpkin_data::entity::EntityPose;
use pumpkin_data::tracked_data::entity::DATA_AIR_SUPPLY_ID;
use sculith_protocol::bedrock::client::set_actor_data::{
    MetadataValue, SyncedActorDataList, entity_data_key,
};
use sculith_protocol::codec::var_int::VarInt;
use sculith_util::GameMode;
use sculith_util::math::{position::BlockPos, vector2::Vector2};
use std::sync::atomic::{AtomicI32, Ordering};

pub const MAX_AIR: i32 = 300;
pub const AIR_RECOVERY_RATE: i32 = 4;
pub const AIR_DEPLETION_RATE: i32 = 1;
pub const DROWNING_INTERVAL: i32 = 20;
pub const DROWNING_DAMAGE: f32 = 2.0;

pub struct BreathManager {
    pub air_supply: AtomicI32,
    pub drowning_tick: AtomicI32,
}

impl Default for BreathManager {
    fn default() -> Self {
        Self {
            air_supply: AtomicI32::new(MAX_AIR),
            drowning_tick: AtomicI32::new(0),
        }
    }
}

impl BreathManager {
    pub fn tick(&self, player: &Player) {
        self.tick_with_deadline(player, None);
    }

    pub(super) fn tick_with_deadline(&self, player: &Player, deadline: Option<TickDeadline>) {
        let client = player
            .client
            .bedrock()
            .filter(|client| client.is_current_protocol());
        let edit = client.map(|_| player.living_entity.hold_effect_edit());
        if let Some(client) = client {
            if client.take_regional_air_dirty() {
                self.publish_air_supply(player);
            }
            if !TickDeadline::claim(client.breath_deadline(), deadline) {
                return;
            }
        }
        let recovering = self.tick_inner(player);
        drop(edit);
        if recovering && deadline.is_some() && client.is_some() {
            Self::offer_regional_recovery(player);
        }
    }

    fn tick_inner(&self, player: &Player) -> bool {
        let mode = player.gamemode.load();

        if matches!(mode, GameMode::Creative | GameMode::Spectator) {
            if self.air_supply.load(Ordering::Relaxed) != MAX_AIR {
                self.air_supply.store(MAX_AIR, Ordering::Relaxed);
                self.publish_air_supply(player);
            }
            self.drowning_tick.store(0, Ordering::Relaxed);
            return false;
        }

        if !player.world().level_info.load().game_rules.drowning_damage {
            return false;
        }

        if player
            .living_entity
            .has_effect(&StatusEffect::WATER_BREATHING)
        {
            if self.air_supply.swap(MAX_AIR, Ordering::Relaxed) != MAX_AIR {
                self.publish_air_supply(player);
            }
            self.drowning_tick.store(0, Ordering::Relaxed);
            return false;
        }

        let in_water = player.get_entity().is_submerged_in_water();
        let prev = self.air_supply.load(Ordering::Relaxed);

        if in_water {
            let mut new_air = (prev - AIR_DEPLETION_RATE).max(0);
            if new_air != prev {
                let server = player.world().server.upgrade();
                if let Some(server) = server {
                    let mut event = crate::plugin::api::events::entity::entity_air_change::EntityAirChangeEvent::new(
                        player.entity_id(),
                        new_air,
                    );
                    server.plugin_manager.fire_blocking(&server, &mut event);
                    if event.cancelled {
                        return false;
                    }
                    new_air = event.amount.clamp(0, MAX_AIR);
                }
                self.air_supply.store(new_air, Ordering::Relaxed);
                self.publish_air_supply(player);
            }

            if new_air <= 0 {
                let t = self.drowning_tick.fetch_add(1, Ordering::Relaxed) + 1;

                if t >= DROWNING_INTERVAL {
                    self.drowning_tick.store(0, Ordering::Relaxed);
                    player
                        .living_entity
                        .damage(player, DROWNING_DAMAGE, DamageType::DROWN);
                }
            }
            false
        } else {
            let mut new_air = Self::recovered_air(prev);
            if new_air != prev {
                let server = player.world().server.upgrade();
                if let Some(server) = server {
                    let mut event = crate::plugin::api::events::entity::entity_air_change::EntityAirChangeEvent::new(
                        player.entity_id(),
                        new_air,
                    );
                    server.plugin_manager.fire_blocking(&server, &mut event);
                    if event.cancelled {
                        return false;
                    }
                    new_air = event.amount.clamp(0, MAX_AIR);
                }
                self.air_supply.store(new_air, Ordering::Relaxed);
                self.publish_air_supply(player);
            }
            self.drowning_tick.store(0, Ordering::Relaxed);
            new_air < MAX_AIR
        }
    }

    fn recovered_air(air: i32) -> i32 {
        air.saturating_add(AIR_RECOVERY_RATE).min(MAX_AIR)
    }

    pub(crate) fn recover_on_region(
        &self,
        world: &World,
        player: &Player,
        deadline: TickDeadline,
        owns: impl Fn(Vector2<i32>) -> bool,
        publish: impl FnOnce(i32, Option<Box<[u8]>>) -> Option<RegionalActorDelivery>,
    ) -> bool {
        let Some(client) = player
            .client
            .bedrock()
            .filter(|client| client.is_current_protocol())
        else {
            return false;
        };
        let living = &player.living_entity;
        let Some(_edit) = living.try_hold_effect_edit() else {
            return true;
        };
        let previous_air = self.air_supply.load(Ordering::Relaxed);
        if !(0..MAX_AIR).contains(&previous_air)
            || !living.is_alive()
            || !matches!(
                player.gamemode.load(),
                GameMode::Survival | GameMode::Adventure
            )
            || !world.level_info.load().game_rules.drowning_damage
        {
            return false;
        }
        match living.try_has_effect(&StatusEffect::WATER_BREATHING) {
            Some(false) => {}
            Some(true) => return false,
            None => return true,
        }
        let entity = player.get_entity();
        let Ok(awaiting) = player.awaiting_teleport.try_lock() else {
            return true;
        };
        let Ok(_position) = entity.bounding_box.index.try_write() else {
            return true;
        };
        if awaiting.is_some() || entity.pose.load() != EntityPose::Standing {
            return false;
        }
        let eyes = BlockPos::floored_v(entity.get_eye_pos());
        if !owns(eyes.chunk_position()) {
            return true;
        }
        world
            .level
            .try_with_block_state(eyes, |_, state| {
                if !is_air(state) {
                    return false;
                }
                if !owns(eyes.chunk_position()) {
                    return true;
                }
                let clock = client.breath_deadline();
                let previous_step = clock.load();
                if !TickDeadline::claim(clock, Some(deadline)) {
                    return true;
                }
                let air = Self::recovered_air(previous_air);
                if !entity
                    .synched_data
                    .try_publish_value(DATA_AIR_SUPPLY_ID, VarInt(air), |java| {
                        let delivery = publish(air, java)?;
                        if !delivery.bedrock_queued {
                            client.mark_regional_air_dirty();
                        }
                        Some(delivery.java_queued)
                    })
                {
                    clock.store(previous_step);
                    return true;
                }
                self.air_supply.store(air, Ordering::Relaxed);
                self.drowning_tick.store(0, Ordering::Relaxed);
                tracing::trace!(
                    entity = entity.entity_id,
                    air,
                    "Recovered air on the player region"
                );
                air < MAX_AIR
            })
            .unwrap_or(true)
    }

    fn offer_regional_recovery(player: &Player) {
        let world = player.world();
        if let Some(client) = player
            .client
            .bedrock()
            .filter(|client| client.is_current_protocol())
            && let Some(registered) = world.get_player_by_uuid(player.gameprofile.id)
            && std::ptr::eq(registered.as_ref(), player)
        {
            world.offer_regional_breath(client.id, &registered);
        }
    }

    pub(crate) fn bedrock_air_metadata(air: i32) -> SyncedActorDataList {
        let mut metadata = SyncedActorDataList::new();
        metadata.set(
            entity_data_key::AIR_SUPPLY,
            MetadataValue::Short(air as i16),
        );
        metadata
    }

    fn publish_air_supply(&self, player: &Player) {
        let air = self.air_supply.load(Ordering::Relaxed).clamp(0, MAX_AIR);
        let entity = player.get_entity();
        entity.set_synced_data(DATA_AIR_SUPPLY_ID, VarInt(air));
        entity.send_bedrock_actor_data(&Self::bedrock_air_metadata(air));
    }

    pub fn send_air_supply(&self, player: &Player) {
        let _edit = player.living_entity.hold_effect_edit();
        self.publish_air_supply(player);
    }

    pub fn set_air_supply(&self, player: &Player, air: i32) {
        let edit = player.living_entity.hold_effect_edit();
        self.air_supply.store(air, Ordering::Relaxed);
        self.publish_air_supply(player);
        drop(edit);
        if (0..MAX_AIR).contains(&air) {
            Self::offer_regional_recovery(player);
        }
    }

    pub fn reset(&self, player: &Player) {
        let _edit = player.living_entity.hold_effect_edit();
        self.air_supply.store(MAX_AIR, Ordering::Relaxed);
        self.drowning_tick.store(0, Ordering::Relaxed);
        self.publish_air_supply(player);
    }
}
