#[allow(clippy::wildcard_imports)]
use super::*;
use crate::plugin::server::packet::BedrockConnectionPacketSentEvent;
use sculith_protocol::{
    RawPacket, bedrock::server::player_hotbar::SPlayerHotbar, packet::Packet, serial::PacketRead,
};

const REGIONAL_HOTBAR_BYTES: usize = 512;

impl BedrockClient {
    pub(in crate::net::bedrock) const fn can_route_hotbar(packet: &RawPacket) -> bool {
        matches!(
            packet.id,
            SMobEquipment::PACKET_ID | SPlayerHotbar::PACKET_ID
        ) && packet.payload.len() <= REGIONAL_HOTBAR_BYTES
    }

    pub(in crate::net::bedrock) fn try_regional_hotbar_selection(
        &self,
        player: &Player,
        server: &Server,
        packet: &RawPacket,
        owns_player: impl FnOnce() -> bool,
    ) -> Option<()> {
        if !Self::can_route_hotbar(packet)
            || !player.has_client_loaded()
            || self.inventory_opened.load(Ordering::Relaxed)
            || server.stop_token.is_cancelled()
            || server.tick_rate_manager.is_frozen()
            || server.plugin_manager.has_handlers::<PlayerItemHeldEvent>()
            || server
                .plugin_manager
                .has_handlers::<BedrockConnectionPacketSentEvent>()
        {
            return None;
        }

        let mut payload = packet.payload.as_ref();
        let slot = if packet.id == SMobEquipment::PACKET_ID {
            let input = SMobEquipment::read(&mut payload).ok()?;
            if input.entity_runtime_id.0 != player.entity_id() as u64
                || input.container_id != WINDOW_ID_INVENTORY as u8
                || input.slot != input.selected_slot
            {
                return None;
            }
            input.selected_slot
        } else {
            let input = SPlayerHotbar::read(&mut payload).ok()?;
            if !input.should_select_slot || input.container_id != WINDOW_ID_INVENTORY as u8 {
                return None;
            }
            u8::try_from(input.selected_slot.0).ok()?
        };
        if !payload.is_empty() || !PlayerInventory::is_valid_hotbar_index(usize::from(slot)) {
            return None;
        }

        let _edit = player.try_regional_inventory_edit()?;
        let screen = player.current_screen_handler.try_lock().ok()?;
        if !std::ptr::addr_eq(
            Arc::as_ptr(&screen),
            Arc::as_ptr(&player.player_screen_handler),
        ) {
            return None;
        }
        let _handler = screen.try_lock().ok()?;
        let inventory = &player.inventory;
        let main = inventory.main_inventory.try_read().ok()?;
        let previous = inventory.get_selected_slot();
        let stack = &main[usize::from(slot)];
        self.commit_regional_main_hand(
            player,
            server,
            &main[usize::from(previous)],
            stack,
            true,
            || {
                if !owns_player() || !player.has_stable_world_membership() {
                    return None;
                }
                inventory.set_selected_slot(slot);
                player.update_last_action_time();
                tracing::trace!(player = %player.gameprofile.name, slot, "Selected Bedrock hotbar slot on its region");
                Some(())
            },
        )
    }

    /// Applies a simple held item change while the region holds the player's inventory edit guard.
    pub(super) fn commit_regional_main_hand(
        &self,
        player: &Player,
        server: &Server,
        previous: &ItemStack,
        next: &ItemStack,
        publish_equipment: bool,
        apply: impl FnOnce() -> Option<()>,
    ) -> Option<()> {
        if self.food_use_slot().is_some() {
            return None;
        }
        let _checkpoint = server.try_regional_entity_save_guard()?;
        player
            .living_entity
            .with_regional_main_hand_change(previous, next, || {
                if !publish_equipment {
                    return apply();
                }
                let world = player.world();
                world
                    .entity_tracker
                    .try_publish_regional_main_hand(&world, player, next, apply)
            })?
    }

    pub fn select_hotbar_slot(
        &self,
        server: &Arc<Server>,
        player: &Arc<Player>,
        slot: u8,
        container_id: u8,
    ) {
        player.update_last_action_time();
        if slot >= 9 {
            return;
        }
        let previous_slot = player.inventory.get_selected_slot();
        let mut event = PlayerItemHeldEvent::new(player.clone(), previous_slot, slot);
        server.plugin_manager.fire_blocking(server, &mut event);
        if event.cancelled {
            self.try_enqueue_client_packet(&CPlayerHotbar {
                selected_slot: VarUInt(u32::from(previous_slot)),
                container_id,
                should_select_slot: true,
            });
            return;
        }

        if slot != previous_slot && self.food_use_slot().is_some() {
            player.living_entity.clear_active_hand();
        }
        let inv = player.inventory();
        inv.set_selected_slot(slot);
        let stack = inv.held_item();
        let equipment = &[(EquipmentSlot::MAIN_HAND, stack)];
        player.living_entity.send_equipment_changes(equipment);
        player.update_bedrock_shield_sneaking();
    }

    pub fn handle_mob_equipment(
        &self,
        server: &Arc<Server>,
        player: &Arc<Player>,
        packet: &SMobEquipment,
    ) {
        self.select_hotbar_slot(server, player, packet.selected_slot, packet.container_id);
    }
}
