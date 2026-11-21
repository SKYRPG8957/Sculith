use std::sync::Arc;

use pumpkin_data::{Block, data_component_impl::EquipmentSlot, item_stack::ItemStack};
use pumpkin_inventory::player::player_inventory::PlayerInventory;
use pumpkin_protocol::{
    codec::item_stack_seralizer::ItemStackSerializer, java::client::play::CSetPlayerInventory,
};
use pumpkin_util::{GameMode, Hand};

use crate::{
    block::BlockPlacement,
    plugin::player::player_interact_event::{InteractAction, PlayerInteractEvent},
    server::Server,
    world::World,
};

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

    pub(crate) const fn world(&self) -> &Arc<World> {
        &self.world
    }

    /// Checks the clicked block and held item after the interaction callback.
    pub(crate) fn block_interaction(
        &self,
        player: &Arc<Player>,
        server: &Arc<Server>,
        placement: &BlockPlacement,
    ) -> Option<&'static Block> {
        let position = placement.clicked_pos;
        let adjacent = position.offset(placement.clicked_face.to_offset());
        let (block, state) = self.world.get_block_and_state_id(&position);
        let accepted = if server.plugin_manager.has_handlers::<PlayerInteractEvent>() {
            let adjacent_state = self.world.get_block_state_id(&adjacent);
            let mut event = PlayerInteractEvent::new(
                player,
                InteractAction::RightClickBlock,
                block,
                Some(position),
            );
            server.plugin_manager.fire_blocking(server, &mut event);
            !event.cancelled
                && self.world.get_block_state_id(&position) == state
                && self.world.get_block_state_id(&adjacent) == adjacent_state
        } else {
            true
        };

        let in_reach = player.can_interact_with_block_at(&position, 1.0);
        if accepted && in_reach && self.unchanged(player) {
            return Some(block);
        }

        if in_reach && Arc::ptr_eq(&self.world, &player.world()) {
            player.reset_block_change(position);
            player.reset_block_change(adjacent);
        }
        self.correct(player);
        None
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
