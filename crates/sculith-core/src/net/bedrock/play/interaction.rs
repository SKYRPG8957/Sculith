#[allow(clippy::wildcard_imports)]
use super::*;

use crate::plugin::{
    player::inventory_close::InventoryCloseEvent, server::packet::BedrockConnectionPacketSentEvent,
};
use pumpkin_data::data_component_impl::BlocksAttacksImpl;
use pumpkin_inventory::player::player_screen_handler::PlayerScreenHandler;
use sculith_protocol::{RawPacket, packet::Packet, serial::PacketRead};

const fn personal_inventory_open_packet() -> CContainerOpen {
    CContainerOpen {
        container_id: 0,
        container_type: 0xff,
        position: BlockPos::ZERO,
        target_entity_id: VarLong(-1),
    }
}

impl BedrockClient {
    pub(in crate::net::bedrock) fn can_route_inventory_screen(packet: &RawPacket) -> bool {
        // Action, runtime ID and optional hit position, or three close fields.
        (packet.id == SInteract::PACKET_ID
            && packet.payload.first() == Some(&(Action::OpenInventory as u8))
            && packet.payload.len() <= 24)
            || (packet.id == SContainerClose::PACKET_ID && packet.payload.len() == 3)
    }

    pub(in crate::net::bedrock) fn try_regional_inventory_screen(
        &self,
        player: &Player,
        server: &Server,
        packet: &RawPacket,
        owns_player: impl FnOnce() -> bool,
    ) -> Option<()> {
        if !Self::can_route_inventory_screen(packet)
            || !player.has_client_loaded()
            || player.living_entity.is_dead_or_dying()
            || server.stop_token.is_cancelled()
            || server.tick_rate_manager.is_frozen()
            || server
                .plugin_manager
                .has_handlers::<BedrockConnectionPacketSentEvent>()
        {
            return None;
        }
        let _screen_edit = player.try_regional_screen_edit()?;
        let vehicle = player.get_entity().vehicle.try_lock().ok()?;
        let screen = player.current_screen_handler.try_lock().ok()?;
        if vehicle.is_some()
            || player.open_container_pos.load().is_some()
            || !std::ptr::addr_eq(
                Arc::as_ptr(&screen),
                Arc::as_ptr(&player.player_screen_handler),
            )
        {
            return None;
        }
        let mut handler = screen.try_lock().ok()?;
        let current = handler.as_any_mut().downcast_mut::<PlayerScreenHandler>()?;
        let can_apply = || owns_player() && player.has_stable_world_membership();
        let mut payload = packet.payload.as_ref();
        if packet.id == SInteract::PACKET_ID {
            let input = SInteract::read(&mut payload).ok()?;
            if !payload.is_empty() || !matches!(input.action, Action::OpenInventory) || !can_apply()
            {
                return None;
            }
            if !self.inventory_opened.load(Ordering::Relaxed) {
                let reply = self
                    .serialize_packet(&personal_inventory_open_packet())
                    .ok()?;
                self.inventory_opened.store(true, Ordering::Relaxed);
                self.try_enqueue_packet_data_unobserved(reply);
                tracing::trace!(player = %player.gameprofile.name, "Opened Bedrock personal inventory on its region");
            }
            Some(())
        } else {
            let input = SContainerClose::read(&mut payload).ok()?;
            if !matches!(input.container_id, 0 | 0xff)
                || input.server_initiated_close
                || server.plugin_manager.has_handlers::<InventoryCloseEvent>()
            {
                return None;
            }
            self.try_close_regional_inventory(player, current, &input, can_apply)
        }
    }

    fn try_close_regional_inventory(
        &self,
        player: &Player,
        screen: &mut PlayerScreenHandler,
        packet: &SContainerClose,
        can_apply: impl FnOnce() -> bool,
    ) -> Option<()> {
        let cursor = screen.get_behaviour().cursor_stack.clone();
        let crafting = screen.try_empty_crafting_grid()?;
        let mut cursor = cursor.try_lock().ok()?;
        let active = player.living_entity.item_in_use.try_lock().ok()?;
        if player.get_entity().is_sneaking()
            || active
                .as_ref()
                .is_some_and(|stack| stack.get_data_component::<BlocksAttacksImpl>().is_some())
        {
            return None;
        }
        let mut drag = self.cursor_drag.try_lock().ok()?;
        let inventory = &player.inventory;
        let mut main = inventory.main_inventory.try_write().ok()?;
        let equipment = inventory.entity_equipment.try_lock().ok()?;
        let selected = inventory.get_selected_slot();
        let changes = if cursor.is_empty() {
            Vec::new()
        } else {
            let off_hand = inventory
                .equipment_slots
                .get(&PlayerInventory::OFF_HAND_SLOT)
                .map_or_else(|| ItemStack::EMPTY.clone(), |slot| equipment.get(slot));
            PlayerInventory::plan_offer_to_storage(&main, selected, &off_hand, &cursor)?
        };
        let slots = main.iter().enumerate().map(|(index, stack)| {
            changes
                .iter()
                .find(|(slot, _)| *slot == index)
                .map_or(stack, |(_, updated)| updated)
        });
        let changed_slots = changes
            .iter()
            .map(|(slot, stack)| self.encode_player_inventory_slot(*slot, stack))
            .collect::<Option<Vec<_>>>()?;
        let reply = self.serialize_packet(packet).ok()?;
        let packets =
            self.encode_personal_inventory(inventory, &equipment, slots, ItemStack::EMPTY)?;
        if inventory.get_selected_slot() != selected || !can_apply() {
            return None;
        }
        let returned_slots = changes.len();
        if !changes.is_empty() {
            for (slot, stack) in &changes {
                main[*slot] = stack.clone();
            }
            *cursor = ItemStack::EMPTY.clone();
            player.inventory_changed.store(true, Ordering::Relaxed);
            drop(crafting);
            for (slot, stack) in changes {
                if let Some(screen_slot) =
                    map_bedrock_slot_to_screen_handler(WINDOW_ID_INVENTORY, slot as u32)
                {
                    screen.set_received_stack(screen_slot, stack);
                }
            }
            screen
                .get_behaviour_mut()
                .previous_cursor_stack
                .set_received_stack(ItemStack::EMPTY.clone());
        }
        *drag = None;
        self.inventory_opened.store(false, Ordering::Relaxed);
        for bytes in changed_slots.into_iter().chain([reply]).chain(packets) {
            if !self.try_enqueue_packet_data_unobserved(bytes) {
                break;
            }
        }
        tracing::trace!(player = %player.gameprofile.name, returned_slots, "Closed Bedrock personal inventory on its region");
        Some(())
    }

    pub fn handle_interaction(&self, player: &Arc<Player>, packet: &SInteract) {
        match packet.action {
            Action::OpenInventory => {
                if player.open_vehicle_inventory() {
                    return;
                }
                if self.inventory_opened.load(Ordering::Relaxed) {
                    return;
                }
                self.inventory_opened.store(true, Ordering::Relaxed);
                self.try_enqueue_client_packet(&personal_inventory_open_packet());
            }
            Action::StopRiding => {
                if let Some(vehicle) = player.get_entity().get_vehicle() {
                    vehicle.get_entity().remove_passenger(player.entity_id());
                }
            }
            _ => {}
        }
    }
}
