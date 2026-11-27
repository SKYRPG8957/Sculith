use std::sync::Arc;

use pumpkin_data::{BlockDirection, item_stack::ItemStack};
use pumpkin_inventory::player::player_inventory::PlayerInventory;
use sculith_protocol::bedrock::{
    client::item_stack_response::CItemStackResponse,
    network_item::{ContainerName, FullContainerName, NetworkItemDescriptor},
    server::inventory_transaction::{
        HandSlot, LegacySetItemSlot, SInventoryTransaction, TransactionData, WINDOW_ID_ARMOUR,
        WINDOW_ID_INVENTORY, WINDOW_ID_OFF_HAND,
    },
};
use sculith_util::GameMode;

use crate::{entity::player::Player, net::bedrock::BedrockClient};

use super::{
    item_stack_request::{SlotUpdate, build_stack_response, record_update},
    player_inventory::matches_item_type_and_count,
};

fn queried_stack(player: &Player, container: u8, slot: u8) -> Option<(ContainerName, ItemStack)> {
    let name = match i32::from(container) {
        WINDOW_ID_ARMOUR => ContainerName::Armor,
        WINDOW_ID_OFF_HAND => ContainerName::Offhand,
        _ => ContainerName::try_from(container).ok()?,
    };
    let window = match name {
        ContainerName::Inventory | ContainerName::CombinedHotBarAndInventory => WINDOW_ID_INVENTORY,
        ContainerName::HotBar if usize::from(slot) < PlayerInventory::HOTBAR_SIZE => {
            WINDOW_ID_INVENTORY
        }
        ContainerName::Armor => WINDOW_ID_ARMOUR,
        ContainerName::Offhand => WINDOW_ID_OFF_HAND,
        ContainerName::Cursor if slot == 0 => {
            let screen = player
                .current_screen_handler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let cursor = screen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_behaviour()
                .cursor_stack
                .clone();
            let stack = cursor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            return Some((name, stack));
        }
        _ => return None,
    };
    let index = super::map_bedrock_slot_to_screen_handler(window, u32::from(slot))?;
    let stack = player
        .player_screen_handler
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_slot(index)
        .get_cloned_stack();
    Some((name, stack))
}

fn query_updates(player: &Player, groups: &[LegacySetItemSlot]) -> Option<Vec<SlotUpdate>> {
    if groups.len() > 2 || groups.iter().any(|group| group.slots.len() > 2) {
        return None;
    }
    let mut updates = Vec::new();
    for group in groups {
        for &slot in &group.slots {
            let (name, stack) = queried_stack(player, group.container_id, slot)?;
            record_update(
                &mut updates,
                FullContainerName {
                    container_name: name,
                    dynamic_id: None,
                },
                slot,
                &stack,
            );
        }
    }
    Some(updates)
}

impl BedrockClient {
    pub fn handle_inventory_action(&self, player: &Arc<Player>, packet: SInventoryTransaction) {
        tracing::debug!("handle_inventory_action: packet={packet:?}");
        let mut updates = Vec::new();
        let accepted = if query_updates(player, &packet.legacy_set_item_slots).is_none() {
            false
        } else {
            match packet.transaction_data {
                TransactionData::Normal(_) => {
                    if packet.actions.is_empty() {
                        true
                    } else if let Some(changes) =
                        Self::apply_legacy_inventory_drop(player, &packet.actions)
                    {
                        updates = changes;
                        true
                    } else {
                        false
                    }
                }
                TransactionData::Mismatch(_) => packet.actions.is_empty(),
                data => {
                    let allowed = self.validate_item_transaction(player, &data);
                    if allowed {
                        // These actions describe client predictions, not inventory writes.
                        self.handle_item_transaction(player, data);
                    } else {
                        Self::correct_transaction_block_prediction(player, &data);
                    }
                    allowed
                }
            }
        };
        if accepted && let Some(queried) = query_updates(player, &packet.legacy_set_item_slots) {
            for update in queried {
                if let Some(previous) = updates.iter_mut().find(|previous| {
                    previous.container_name == update.container_name
                        && previous.slot_id == update.slot_id
                }) {
                    *previous = update;
                } else {
                    updates.push(update);
                }
            }
        }
        if packet.legacy_request_id.0 != 0 {
            self.try_enqueue_client_packet(&CItemStackResponse {
                responses: vec![build_stack_response(
                    packet.legacy_request_id,
                    u8::from(!accepted),
                    updates,
                )],
            });
        }
        self.resend_player_inventory(player);
    }

    fn validate_item_transaction(&self, player: &Arc<Player>, data: &TransactionData) -> bool {
        let spectator = player.gamemode.load() == GameMode::Spectator;
        let (slot, hand, held) = match data {
            TransactionData::UseItem(data) => {
                match data.action_type.0 {
                    0 if data.hand == HandSlot::Mainhand
                        && data.block_face < 6
                        && player.can_interact_with_block_at(&data.block_position, 1.0) => {}
                    1 if !spectator => {}
                    _ => return false,
                }
                (data.hot_bar_slot.0, data.hand, &data.item_in_hand)
            }
            TransactionData::UseItemOnEntity(data)
                if !spectator
                    && data.hand == HandSlot::Mainhand
                    && matches!(data.action_type.0, 0..=2) =>
            {
                (data.hot_bar_slot.0, data.hand, &data.item_in_hand)
            }
            TransactionData::ReleaseItem(data) if !spectator && data.action_type.0 == 0 => {
                (data.hot_bar_slot.0, data.hand, &data.item_in_hand)
            }
            _ => return false,
        };
        self.validate_transaction_hand(player, slot, hand, held)
    }

    fn validate_transaction_hand(
        &self,
        player: &Arc<Player>,
        slot: i32,
        hand: HandSlot,
        held: &NetworkItemDescriptor,
    ) -> bool {
        let Ok(slot) = u8::try_from(slot) else {
            return false;
        };
        if usize::from(slot) >= PlayerInventory::HOTBAR_SIZE {
            return false;
        }
        let index = if hand == HandSlot::Mainhand {
            usize::from(slot)
        } else {
            PlayerInventory::OFF_HAND_SLOT
        };
        let before = player.inventory.get_slot(index);
        if !matches_item_type_and_count(held, &before) {
            return false;
        }
        let world = player.world();
        let mode = player.gamemode.load();
        if hand == HandSlot::Mainhand && player.inventory.get_selected_slot() != slot {
            let Some(server) = world.server.upgrade() else {
                return false;
            };
            self.select_hotbar_slot(&server, player, slot, WINDOW_ID_INVENTORY as u8);
            if player.inventory.get_selected_slot() != slot {
                return false;
            }
        }
        let current = player.inventory.get_slot(index);
        Arc::ptr_eq(&world, &player.world())
            && mode == player.gamemode.load()
            && before.uid == current.uid
            && before.are_equal(&current)
    }

    pub(super) fn correct_transaction_block_prediction(player: &Player, data: &TransactionData) {
        let TransactionData::UseItem(data) = data else {
            return;
        };
        if data.action_type.0 != 0 || !player.can_interact_with_block_at(&data.block_position, 1.0)
        {
            return;
        }
        let directions = [
            BlockDirection::Down,
            BlockDirection::Up,
            BlockDirection::North,
            BlockDirection::South,
            BlockDirection::West,
            BlockDirection::East,
        ];
        let Some(face) = directions.get(usize::from(data.block_face)) else {
            return;
        };
        player.reset_block_change(data.block_position);
        player.reset_block_change(data.block_position.offset(face.to_offset()));
    }
}
