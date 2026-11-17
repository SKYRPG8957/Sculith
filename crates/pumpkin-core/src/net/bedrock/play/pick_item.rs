#[allow(clippy::wildcard_imports)]
use super::*;

impl BedrockClient {
    pub fn handle_block_pick_request(&self, player: &Arc<Player>, packet: &SBlockPickRequest) {
        if !player.can_interact_with_block_at(&packet.position, 1.0) {
            return;
        }

        let world = player.world();
        let block = world.get_block(&packet.position);

        if block.item_id == 0 {
            return;
        }

        let Some(item) = pumpkin_data::item::Item::from_id(block.item_id) else {
            return;
        };
        let stack = ItemStack::new(1, item);
        self.pick_item_stack(player, stack);
    }

    pub fn handle_actor_pick_request(&self, player: &Arc<Player>, packet: &SActorPickRequest) {
        if player.gamemode.load() != GameMode::Creative {
            return;
        }
        let Ok(id) = i32::try_from(packet.entity_unique_id) else {
            return;
        };
        let Some(target) = player.world().get_entity_by_id(id) else {
            return;
        };
        if !player.can_interact_with_entity(target.as_ref(), 1.0) {
            return;
        }

        let target_type = target.get_entity().entity_type;
        let item = pumpkin_data::entity::spawn_egg_ids()
            .iter()
            .find(|&&id| {
                pumpkin_data::entity::entity_from_egg(id)
                    .is_some_and(|entity_type| entity_type.id == target_type.id)
            })
            .and_then(|&id| pumpkin_data::item::Item::from_id(id));
        if let Some(item) = item {
            self.pick_item_stack(player, ItemStack::new(1, item));
        }
    }

    fn pick_item_stack(&self, player: &Arc<Player>, stack: ItemStack) {
        {
            let inventory = player.inventory();
            let mut contents = inventory
                .main_inventory
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut selected = inventory.get_selected_slot() as usize;

            // Match Dragonfly's pick selection; the client hint is not a destination slot.
            if let Some(slot) = contents
                .iter()
                .position(|item| !item.is_empty() && item.are_items_and_components_equal(&stack))
            {
                if PlayerInventory::is_valid_hotbar_index(slot) {
                    inventory.set_selected_slot(slot as u8);
                } else {
                    contents.swap(selected, slot);
                }
            } else if player.gamemode.load() == GameMode::Creative {
                if let Some(empty) = contents.iter().position(ItemStack::is_empty) {
                    if PlayerInventory::is_valid_hotbar_index(empty) {
                        selected = empty;
                        inventory.set_selected_slot(empty as u8);
                    } else {
                        contents.swap(selected, empty);
                    }
                }
                contents[selected] = stack;
            } else {
                return;
            }
        }

        // Send hotbar updates
        player.client.try_enqueue_packet_editioned(
            &CSetSelectedSlot::new(player.inventory.get_selected_slot() as i8),
            &CPlayerHotbar {
                selected_slot: VarUInt(player.inventory.get_selected_slot() as u32),
                container_id: 0,
                should_select_slot: true,
            },
        );

        // Send screen handler / Java inventory updates
        player
            .player_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .send_content_updates();

        // Sync main hand equipment to other players
        let stack_in_hand = player.inventory().held_item();
        let equipment = &[(EquipmentSlot::MAIN_HAND, stack_in_hand)];
        player.living_entity.send_equipment_changes(equipment);

        // Sync bedrock inventory updates
        let slots = player
            .inventory()
            .main_inventory
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(NetworkItemStackDescriptor::from)
            .collect();
        self.try_enqueue_client_packet(&CInventoryContent {
            container_id: VarUInt(0),
            slots,
            full_container_name: FullContainerName {
                container_name: ContainerName::Inventory,
                dynamic_id: None,
            },
            storage_item: NetworkItemStackDescriptor::default(),
        });
    }
}
