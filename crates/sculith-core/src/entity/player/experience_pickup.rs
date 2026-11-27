use std::sync::Arc;

use pumpkin_data::{enchantment::Enchantment, item_stack::ItemStack};
use pumpkin_inventory::player::player_inventory::PlayerInventory;

use crate::net::bedrock::BedrockClient;
use crate::server::{Server, TickDeadline};

use super::Player;

const PICKUP_DELAY_TICKS: u32 = 2;

impl Player {
    pub(crate) fn begin_experience_pickup(self: &Arc<Self>) -> bool {
        let Ok(mut delay) = self.experience_pick_up_delay.try_lock() else {
            return false;
        };
        if *delay > 0 {
            return false;
        }
        *delay = PICKUP_DELAY_TICKS;
        if let Some(client) = self
            .client
            .bedrock()
            .filter(|client| client.is_current_protocol())
        {
            client.start_experience_pickup_clock();
            drop(delay);
            self.world()
                .offer_regional_experience_pickup(client.id, self);
        }
        true
    }

    pub(super) fn can_mend_with_experience(stack: &ItemStack) -> bool {
        stack.get_enchantment_level(&Enchantment::MENDING) > 0 && stack.get_damage() > 0
    }

    /// Keeps the held item and equipment stable while awarding XP to an unmounted collector.
    pub(crate) fn try_collect_regional_experience(
        self: &Arc<Self>,
        client: &BedrockClient,
        amount: i32,
        remove: impl FnOnce() -> bool,
    ) -> Option<()> {
        let mut delay = self.experience_pick_up_delay.try_lock().ok()?;
        if *delay > 0 {
            return None;
        }
        let main = self.inventory.main_inventory.try_read().ok()?;
        let equipped = self.inventory.entity_equipment.try_lock().ok()?;
        let selected = usize::from(self.inventory.get_selected_slot());
        let held = main.get(selected)?;
        if Self::can_mend_with_experience(held)
            || self.inventory.equipment_slots.values().any(|slot| {
                (slot.is_armor_slot()
                    || self
                        .inventory
                        .equipment_slots
                        .get(&PlayerInventory::OFF_HAND_SLOT)
                        == Some(slot))
                    && Self::can_mend_with_experience(&equipped.get(slot))
            })
        {
            return None;
        }
        self.try_award_regional_experience(amount, remove)?;
        *delay = PICKUP_DELAY_TICKS;
        client.start_experience_pickup_clock();
        drop(equipped);
        drop(main);
        drop(delay);
        self.world()
            .offer_regional_experience_pickup(client.id, self);
        Some(())
    }

    pub(super) fn tick_experience_pickup(&self, server: &Server, deadline: Option<TickDeadline>) {
        let Ok(mut delay) = self.experience_pick_up_delay.try_lock() else {
            return;
        };
        if *delay == 0 {
            return;
        }
        let Some(client) = self
            .client
            .bedrock()
            .filter(|client| client.is_current_protocol())
        else {
            *delay -= 1;
            return;
        };
        if deadline.is_some()
            && let Some(_native) = server.plugin_manager.try_without_native_code()
        {
            let world = self.world();
            if let Some(player) = world.get_player_by_uuid(self.gameprofile.id)
                && std::ptr::eq(player.as_ref(), self)
                && world.offer_regional_experience_pickup(client.id, &player)
            {
                return;
            }
        }
        client.tick_experience_pickup_delay(&mut delay, deadline);
    }
}
