use std::sync::Arc;

use pumpkin_data::{data_component_impl::EquipmentSlot, item_stack::ItemStack};
use pumpkin_inventory::player::player_inventory::PlayerInventory;
use pumpkin_protocol::{
    codec::item_stack_seralizer::ItemStackSerializer, java::client::play::CSetPlayerInventory,
};
use pumpkin_util::{GameMode, Hand};

use crate::world::World;

use super::Player;

pub struct ItemUseSnapshot {
    pub(crate) stack: ItemStack,
    hand: Hand,
    selected: u8,
    mode: GameMode,
    world: Arc<World>,
}

impl ItemUseSnapshot {
    pub(crate) fn new(player: &Player, hand: Hand) -> Self {
        Self {
            stack: player.inventory.get_stack_in_hand(hand),
            hand,
            selected: player.inventory.get_selected_slot(),
            mode: player.gamemode.load(),
            world: player.world(),
        }
    }

    pub(crate) fn unchanged(&self, player: &Player) -> bool {
        let current = player.inventory.get_stack_in_hand(self.hand);
        Arc::ptr_eq(&self.world, &player.world())
            && self.mode == player.gamemode.load()
            && self.selected == player.inventory.get_selected_slot()
            && self.stack.uid == current.uid
            && self.stack.are_equal(&current)
    }

    pub(crate) fn correct(&self, player: &Player) {
        let (slot, equipment) = match self.hand {
            Hand::Right => (usize::from(self.selected), EquipmentSlot::MAIN_HAND),
            Hand::Left => (PlayerInventory::OFF_HAND_SLOT, EquipmentSlot::OFF_HAND),
        };
        let stack = player.inventory.get_slot(slot);
        player.try_send_slot_set_packet(&CSetPlayerInventory::new(
            (slot as i32).into(),
            &ItemStackSerializer::from(stack.clone()),
        ));
        if self.hand == Hand::Left || self.selected == player.inventory.get_selected_slot() {
            player
                .living_entity
                .send_equipment_changes(&[(equipment, stack)]);
        }
        // Correct predicted consumption without clearing use state written by a callback.
        player.send_health();
    }
}
