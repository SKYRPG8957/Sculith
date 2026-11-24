#[allow(clippy::wildcard_imports)]
use super::*;
use sculith_protocol::{
    codec::item_stack_seralizer::ItemStackSerializer, java::client::play::CSetContainerSlot,
};

impl JavaClient {
    pub fn handle_set_creative_slot(
        &self,
        player: &Arc<Player>,
        packet: SSetCreativeSlot,
    ) -> Result<(), InventoryError> {
        if player.gamemode.load() != GameMode::Creative {
            return Err(InventoryError::PermissionError);
        }
        let is_negative = packet.slot < 0;
        let valid_slot = packet.slot >= 1 && packet.slot as usize <= 45;
        let item_stack = packet
            .clicked_item
            .to_stack_for_version(&pumpkin_data::packet::CURRENT_MC_VERSION);
        let is_legal =
            item_stack.is_empty() || item_stack.item_count <= item_stack.get_max_stack_size();
        let before = valid_slot.then(|| {
            player
                .player_screen_handler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_slot(packet.slot as usize)
                .get_cloned_stack()
        });
        let world = player.world();
        let allowed = is_legal && player.allow_creative_change(packet.slot, &item_stack);
        if let Some(before) = before {
            let (current, revision, changed) = {
                let mut handler = player
                    .player_screen_handler
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let slot = handler.get_slot(packet.slot as usize);
                let current = slot.get_cloned_stack();
                if allowed
                    && Arc::ptr_eq(&world, &player.world())
                    && before.uid == current.uid
                    && before.are_equal(&current)
                {
                    slot.set_stack(item_stack);
                }
                let current = slot.get_cloned_stack();
                handler.set_received_stack(packet.slot as usize, current.clone());
                let changed = !before.are_equal(&current);
                (
                    current,
                    handler.get_behaviour().next_revision() as i32,
                    changed,
                )
            };
            if changed {
                player.sync_screen_slot_equipment(packet.slot as usize, &current);
            }
            player.try_send_client_packet(&CSetContainerSlot::new(
                0,
                revision,
                packet.slot,
                &ItemStackSerializer::from(current),
            ));
        } else if is_negative
            && allowed
            && Arc::ptr_eq(&world, &player.world())
            && !item_stack.is_empty()
        {
            player.drop_item(item_stack);
        }
        Ok(())
    }
}
