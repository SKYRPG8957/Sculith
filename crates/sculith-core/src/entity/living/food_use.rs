use pumpkin_data::data_component_impl::{
    AttributeModifiersImpl, BlocksAttacksImpl, EquippableImpl, FoodImpl, KineticWeaponImpl,
    OminousBottleAmplifierImpl, PotionContentsImpl, SuspiciousStewEffectsImpl, UseRemainderImpl,
    food::ConsumableImpl,
};
use pumpkin_data::{item_stack::ItemStack, tracked_data};
use sculith_protocol::bedrock::client::set_actor_data::{
    MetadataValue, SyncedActorDataList, entity_data_flag, entity_data_key,
};
use sculith_util::{GameMode, Hand, math::vector3::Vector3};
use std::sync::atomic::Ordering::Relaxed;

use super::{LivingEntity, RegionalActorDelivery};
use crate::{entity::player::Player, net::bedrock::BedrockClient, server::TickDeadline};

pub enum RegionalFoodUseFinish {
    Pending,
    World,
    Finished(Vector3<f64>),
}

impl LivingEntity {
    /// The caller pins the ordered input, personal screen, region and checkpoint.
    #[expect(
        clippy::too_many_lines,
        reason = "Validate and commit under the same guards"
    )]
    pub(crate) fn finish_food_use_on_region(
        &self,
        player: &Player,
        client: &BedrockClient,
        deadline: TickDeadline,
        can_finish: impl FnOnce(Vector3<f64>) -> bool,
        publish: impl FnOnce(
            SyncedActorDataList,
            Option<Box<[u8]>>,
            bool,
        ) -> Option<RegionalActorDelivery>,
    ) -> RegionalFoodUseFinish {
        use RegionalFoodUseFinish::{Finished, Pending, World};

        let Some(_edit) = self.try_hold_effect_edit() else {
            return Pending;
        };
        let Some(slot) = client.food_use_slot() else {
            return World;
        };
        let mode = player.gamemode.load();
        if self.item_use_time.load(Relaxed) != 1
            || !self.is_alive()
            || !matches!(mode, GameMode::Survival | GameMode::Adventure)
        {
            return World;
        }
        if !client.regional_food_use_due(deadline) {
            return Pending;
        }
        let Ok(_position) = self.entity.bounding_box.index.try_write() else {
            return Pending;
        };
        let (Ok(mut used), Ok(mut hand), Ok(mut kinetic), Ok(advancements), Ok(_abilities)) = (
            self.item_in_use.try_lock(),
            self.active_hand.try_lock(),
            self.recent_kinetic_enemies.try_lock(),
            player.advancements.try_lock(),
            player.abilities.try_lock(),
        ) else {
            return Pending;
        };
        let Some(item) = used.as_ref() else {
            return World;
        };
        let Some(food) = Self::food_without_finish_effects(item) else {
            return World;
        };
        if *hand != Some(Hand::Right)
            || self.entity.bedrock_flags_two.load(Relaxed)
                & (1 << (entity_data_flag::BLOCKING - 64))
                != 0
            || advancements.has_consumption_listener(item.item.registry_key)
        {
            return World;
        }
        let Ok(mut inventory) = player.inventory.main_inventory.try_write() else {
            return Pending;
        };
        let held = &mut inventory[usize::from(slot)];
        if player.inventory.get_selected_slot() != slot || !Self::is_held_food(item, held) {
            return World;
        }

        let mut replacement = held.clone();
        replacement.decrement_unless_creative(mode, 1);
        let empty_main_hand = replacement.is_empty();
        if empty_main_hand
            && held
                .get_data_component::<AttributeModifiersImpl>()
                .is_some_and(|modifiers| !modifiers.attribute_modifiers.is_empty())
        {
            return World;
        }
        let (level, saturation) = player
            .hunger_manager
            .meal_values(food.nutrition as u8, food.saturation);
        let Some(max_health) = self.try_max_health() else {
            return Pending;
        };
        let position = self.entity.pos.load();
        let flags = self.livings_flags.load(Relaxed) & !Self::USING_ITEM_FLAG;
        let (Some(slot_packet), Ok(hud_packet)) = (
            client.encode_player_inventory_slot(usize::from(slot), &replacement),
            client.serialize_packet(&player.bedrock_health_packet(level, saturation, max_health)),
        ) else {
            return World;
        };
        if !can_finish(position) {
            return World;
        }
        if !self.try_publish_food_use_end(client, flags, |metadata, java| {
            publish(metadata, java, empty_main_hand)
        }) {
            return Pending;
        }

        *held = replacement;
        player.inventory_changed.store(true, Relaxed);
        player.hunger_manager.level.store(level);
        player.hunger_manager.saturation.store(saturation);
        self.livings_flags
            .fetch_and(!Self::USING_ITEM_FLAG, Relaxed);
        self.entity
            .bedrock_flags
            .fetch_and(!(1 << entity_data_flag::USING_ITEM), Relaxed);
        self.item_use_time.store(0, Relaxed);
        *used = None;
        *hand = None;
        kinetic.clear();
        client.reset_food_use(None);

        client.try_enqueue_packet_data_unobserved(slot_packet);
        if !client.try_enqueue_packet_data_unobserved(hud_packet) {
            client.mark_regional_food_dirty();
        }
        tracing::trace!(
            entity = self.entity.entity_id,
            level,
            "Finished food use on the player region"
        );
        Finished(position)
    }

    fn try_publish_food_use_end(
        &self,
        client: &BedrockClient,
        flags: u8,
        publish: impl FnOnce(SyncedActorDataList, Option<Box<[u8]>>) -> Option<RegionalActorDelivery>,
    ) -> bool {
        self.entity.synched_data.try_publish_value(
            tracked_data::living_entity::DATA_LIVING_ENTITY_FLAGS,
            flags,
            |java| {
                let bedrock_flags =
                    self.entity.bedrock_flags.load(Relaxed) & !(1 << entity_data_flag::USING_ITEM);
                let mut metadata = SyncedActorDataList::new();
                metadata.set(entity_data_key::FLAGS, MetadataValue::Int64(bedrock_flags));
                let delivery = publish(metadata, java)?;
                if !delivery.bedrock_queued {
                    client.mark_food_use_metadata_dirty();
                }
                Some(delivery.java_queued)
            },
        )
    }

    fn food_without_finish_effects(item: &ItemStack) -> Option<&FoodImpl> {
        let consumable = item.get_data_component::<ConsumableImpl>()?;
        if !consumable.effects.is_empty()
            || item.get_use_cooldown().is_some()
            || item.get_data_component::<UseRemainderImpl>().is_some()
            || item.get_data_component::<BlocksAttacksImpl>().is_some()
            || item.get_data_component::<EquippableImpl>().is_some()
            || item.get_data_component::<KineticWeaponImpl>().is_some()
            || item.get_data_component::<PotionContentsImpl>().is_some()
            || item
                .get_data_component::<OminousBottleAmplifierImpl>()
                .is_some()
            || item
                .get_data_component::<SuspiciousStewEffectsImpl>()
                .is_some()
        {
            return None;
        }
        item.get_data_component::<FoodImpl>()
    }
}
