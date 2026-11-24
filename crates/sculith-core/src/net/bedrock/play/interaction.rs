#[allow(clippy::wildcard_imports)]
use super::*;

use crate::plugin::{
    player::inventory_close::InventoryCloseEvent, server::packet::BedrockConnectionPacketSentEvent,
};
use bytes::Bytes;
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
    fn encode_personal_inventory_open(
        &self,
        inputs: &[ItemStack],
    ) -> std::io::Result<(Bytes, Vec<Bytes>)> {
        let open = self.serialize_packet(&personal_inventory_open_packet())?;
        let updates = if inputs.iter().any(|stack| !stack.is_empty()) {
            inputs
                .iter()
                .enumerate()
                .map(|(index, stack)| self.encode_personal_crafting_input(index, stack))
                .collect::<std::io::Result<_>>()?
        } else {
            Vec::new()
        };
        Ok((open, updates))
    }

    fn send_personal_inventory_open(&self, open: Bytes, updates: Vec<Bytes>, observe: bool) {
        self.inventory_opened.store(true, Ordering::Relaxed);
        for packet in std::iter::once(open).chain(updates) {
            let sent = if observe {
                self.try_enqueue_packet_data(packet)
            } else {
                self.try_enqueue_packet_data_unobserved(packet)
            };
            if !sent {
                break;
            }
        }
    }

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
        let _screen_edit = player.try_regional_inventory_edit()?;
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
            if !payload.is_empty() || !matches!(input.action, Action::OpenInventory) {
                return None;
            }
            if self.inventory_opened.load(Ordering::Relaxed) {
                return can_apply().then_some(());
            }
            let inputs = player.personal_crafting_inventory.items.try_read().ok()?;
            let (open, updates) = self.encode_personal_inventory_open(&inputs).ok()?;
            if !can_apply() {
                return None;
            }
            self.send_personal_inventory_open(open, updates, false);
            tracing::trace!(player = %player.gameprofile.name, "Opened Bedrock personal inventory on its region");
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
        let mut cursor = cursor.try_lock().ok()?;
        let mut crafting = screen.try_crafting_inputs()?;
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
        let returned_inputs = crafting.iter().filter(|stack| !stack.is_empty()).count();
        let changes = if cursor.is_empty() && returned_inputs == 0 {
            Vec::new()
        } else {
            let off_hand = inventory
                .equipment_slots
                .get(&PlayerInventory::OFF_HAND_SLOT)
                .map_or_else(|| ItemStack::EMPTY.clone(), |slot| equipment.get(slot));
            PlayerInventory::plan_offers_to_storage(
                &main,
                selected,
                &off_hand,
                std::iter::once(&*cursor).chain(crafting.iter()),
            )?
        };
        let slots = main.iter().enumerate().map(|(index, stack)| {
            changes
                .iter()
                .rev()
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
        let slot_updates = changes.len();
        if !changes.is_empty() {
            for (slot, stack) in &changes {
                main[*slot] = stack.clone();
            }
            *cursor = ItemStack::EMPTY.clone();
            let grid_size = crafting.len();
            crafting.fill_with(|| ItemStack::EMPTY.clone());
            player.inventory_changed.store(true, Ordering::Relaxed);
            drop(crafting);
            for slot in 1..=grid_size {
                screen.set_received_stack(slot, ItemStack::EMPTY.clone());
            }
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
        tracing::trace!(player = %player.gameprofile.name, slot_updates, returned_inputs, "Closed Bedrock personal inventory on its region");
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
                let personal = {
                    let current = player
                        .current_screen_handler
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    std::ptr::addr_eq(
                        Arc::as_ptr(&current),
                        Arc::as_ptr(&player.player_screen_handler),
                    )
                };
                let encoded = if personal {
                    let inputs = player
                        .personal_crafting_inventory
                        .items
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    self.encode_personal_inventory_open(&inputs)
                } else {
                    self.encode_personal_inventory_open(&[])
                };
                let (open, updates) = match encoded {
                    Ok(packets) => packets,
                    Err(err) => {
                        tracing::error!("Failed to encode Bedrock personal inventory: {err}");
                        return;
                    }
                };
                self.send_personal_inventory_open(open, updates, true);
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
