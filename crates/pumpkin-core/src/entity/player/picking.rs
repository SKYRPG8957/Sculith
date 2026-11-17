use std::sync::{Arc, atomic::Ordering};

use pumpkin_data::{data_component_impl::EquipmentSlot, item_stack::ItemStack};
use pumpkin_inventory::{player::player_inventory::PlayerInventory, screen_handler::ScreenHandler};
use pumpkin_protocol::{
    bedrock::{
        client::{inventory_content::CInventoryContent, player_hotbar::CPlayerHotbar},
        network_item::{ContainerName, FullContainerName, NetworkItemStackDescriptor},
    },
    codec::var_uint::VarUInt,
    java::client::play::CSetSelectedSlot,
};
use pumpkin_util::GameMode;

use crate::{net::ClientPlatform, plugin::player::item_held::PlayerItemHeldEvent};

use super::Player;

#[derive(Clone, Copy, PartialEq, Eq)]
struct PickPlan {
    selected: u8,
    swap: Option<usize>,
    create: bool,
}

impl PickPlan {
    fn find(
        contents: &[ItemStack; PlayerInventory::MAIN_SIZE],
        selected: u8,
        stack: &ItemStack,
        creative: bool,
    ) -> Option<Self> {
        if let Some(slot) = contents
            .iter()
            .position(|item| !item.is_empty() && item.are_items_and_components_equal(stack))
        {
            let hotbar = PlayerInventory::is_valid_hotbar_index(slot);
            return Some(Self {
                selected: if hotbar { slot as u8 } else { selected },
                swap: (!hotbar).then_some(slot),
                create: false,
            });
        }
        if !creative {
            return None;
        }
        let empty = contents.iter().position(ItemStack::is_empty);
        let hotbar = empty.filter(|&slot| PlayerInventory::is_valid_hotbar_index(slot));
        Some(Self {
            selected: hotbar.map_or(selected, |slot| slot as u8),
            swap: empty.filter(|&slot| !PlayerInventory::is_valid_hotbar_index(slot)),
            create: true,
        })
    }
}

impl Player {
    pub(crate) fn pick_item(self: &Arc<Self>, stack: ItemStack) {
        let previous = self.inventory.get_selected_slot();
        let plan = {
            let contents = self
                .inventory
                .main_inventory
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            PickPlan::find(
                &contents,
                previous,
                &stack,
                self.gamemode.load() == GameMode::Creative,
            )
        };
        if let Some(plan) = plan {
            let allowed = if plan.selected == previous {
                true
            } else {
                let Some(server) = self.world().server.upgrade() else {
                    return;
                };
                let mut event = PlayerItemHeldEvent::new(self.clone(), previous, plan.selected);
                server.plugin_manager.fire_blocking(&server, &mut event);
                !event.cancelled
            };
            if allowed {
                let mut contents = self
                    .inventory
                    .main_inventory
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // A plugin may change the inventory while handling the slot event.
                if PickPlan::find(
                    &contents,
                    previous,
                    &stack,
                    self.gamemode.load() == GameMode::Creative,
                ) == Some(plan)
                    && self
                        .inventory
                        .selected_slot
                        .compare_exchange(
                            previous,
                            plan.selected,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                {
                    if let Some(source) = plan.swap {
                        contents.swap(usize::from(plan.selected), source);
                    }
                    if plan.create {
                        contents[usize::from(plan.selected)] = stack;
                    }
                }
            }
        }
        self.sync_picked_inventory();
    }

    fn sync_picked_inventory(&self) {
        let selected = self.inventory.get_selected_slot();
        self.client.try_enqueue_packet_editioned(
            &CSetSelectedSlot::new(selected as i8),
            &CPlayerHotbar {
                selected_slot: VarUInt(u32::from(selected)),
                container_id: 0,
                should_select_slot: true,
            },
        );
        self.player_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sync_state();
        self.living_entity
            .send_equipment_changes(&[(EquipmentSlot::MAIN_HAND, self.inventory.held_item())]);
        self.update_bedrock_shield_sneaking();
        if let ClientPlatform::Bedrock(client) = self.client.as_ref() {
            let slots = self
                .inventory
                .main_inventory
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .map(NetworkItemStackDescriptor::from)
                .collect();
            client.try_enqueue_client_packet(&CInventoryContent {
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
}
