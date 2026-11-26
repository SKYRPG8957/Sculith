use std::sync::{Arc, atomic::Ordering::Relaxed};

use pumpkin_data::{
    Block, BlockDirection, BlockStateId,
    data_component_impl::{BlockStateImpl, BlockTransformerImpl},
    statistic::StatisticCategory,
};
use pumpkin_inventory::player::player_inventory::PlayerInventory;
use sculith_protocol::{
    RawPacket,
    bedrock::server::inventory_transaction::{
        HandSlot, SInventoryTransaction, TransactionData, UseItemTransactionData,
    },
    packet::Packet,
    serial::PacketRead,
};
use sculith_util::{GameMode, math::vector2::Vector2};

use crate::{
    entity::{EntityBase, player::Player},
    net::bedrock::BedrockClient,
    server::Server,
};

use super::player_inventory::matches_item_type_and_count;

const REGIONAL_PLACEMENT_BYTES: usize = 512;

impl BedrockClient {
    pub(in crate::net::bedrock) fn can_route_block_placement(packet: &RawPacket) -> bool {
        packet.id == SInventoryTransaction::PACKET_ID
            && packet.payload.len() <= REGIONAL_PLACEMENT_BYTES
            // No legacy request, legacy slots or inventory actions. The
            // transaction is UseItem, with the click-block action.
            && packet.payload.starts_with(&[0, 0, 2, 0, 0])
    }

    pub(in crate::net::bedrock) fn try_regional_block_placement(
        &self,
        player: &Player,
        server: &Server,
        packet: &RawPacket,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<()> {
        let mut payload = packet.payload.as_ref();
        let packet = SInventoryTransaction::read(&mut payload).ok()?;
        let TransactionData::UseItem(data) = packet.transaction_data else {
            return None;
        };
        if !payload.is_empty() {
            return None;
        }
        self.try_apply_regional_placement(player, server, &data, owns)
    }

    fn try_apply_regional_placement(
        &self,
        player: &Player,
        server: &Server,
        data: &UseItemTransactionData,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<()> {
        let face = BlockDirection::from_index(data.block_face)?;
        let slot = usize::try_from(data.hot_bar_slot.0).ok()?;
        let mode = player.gamemode.load();
        if data.action_type.0 != 0
            || data.hand != HandSlot::Mainhand
            || slot >= PlayerInventory::HOTBAR_SIZE
            || !matches!(mode, GameMode::Survival | GameMode::Creative)
            || self.inventory_opened.load(Relaxed)
        {
            return None;
        }
        let world = player.world();
        let entity = player.get_entity();
        let (position, state) = {
            let _edit = player.try_regional_inventory_edit()?;
            let teleport = player.awaiting_teleport.try_lock().ok()?;
            if teleport.is_some() || !self.inventory_request_can_run_on_region(player, server) {
                return None;
            }
            entity.bounding_box.try_with_current_world(&world, || {
                let clicked = data.block_position;
                let eligible = || {
                    !self.is_closed()
                        && player.can_simulate_in(&world)
                        && player.has_stable_world_membership()
                        && player.gamemode.load() == mode
                        && usize::from(player.inventory.get_selected_slot()) == slot
                        && owns(entity.chunk_pos.load())
                        && owns(clicked.chunk_position())
                };
                if !eligible() || !player.try_bedrock_block_reach(&clicked, mode, 1.0)? {
                    return None;
                }
                let position = clicked.offset(face.to_offset());
                if !owns(position.chunk_position())
                    || world.level.try_read_block_state(position) != Some(BlockStateId::AIR)
                {
                    return None;
                }
                let state = self.try_commit_regional_placement(
                    player,
                    server,
                    data,
                    slot,
                    mode,
                    |state| {
                        world.try_place_regional_block(
                            server,
                            player,
                            clicked,
                            position,
                            state,
                            &|chunk| owns(chunk) && eligible(),
                        )
                    },
                )?;
                Some((position, state))
            })??
        };
        world.publish_regional_placement(position, state);
        tracing::trace!(player = %player.gameprofile.name, ?position, "Placed Bedrock block on its region");
        Some(())
    }

    fn try_commit_regional_placement(
        &self,
        player: &Player,
        server: &Server,
        data: &UseItemTransactionData,
        slot: usize,
        mode: GameMode,
        place: impl FnOnce(BlockStateId) -> Option<()>,
    ) -> Option<BlockStateId> {
        let current = player.current_screen_handler.try_lock().ok()?;
        if !std::ptr::addr_eq(
            Arc::as_ptr(&current),
            Arc::as_ptr(&player.player_screen_handler),
        ) {
            return None;
        }
        let handler = current.try_lock().ok()?;
        let cursor = handler.get_behaviour().cursor_stack.try_lock().ok()?;
        let inventory = &player.inventory;
        let mut main = inventory.main_inventory.try_write().ok()?;
        let held = main.get(slot)?.clone();
        if held.is_empty()
            || !matches_item_type_and_count(&data.item_in_hand, &held)
            || held.get_data_component::<BlockStateImpl>().is_some()
            || held.get_data_component::<BlockTransformerImpl>().is_some()
            || server
                .item_registry
                .get_pumpkin_item(held.item.id)
                .is_some()
        {
            return None;
        }
        let block = Block::from_item_id(held.item.id)?;
        let state = block.default_state.id;
        if sculith_world::lighting::LightEngine::has_different_light_properties(
            BlockStateId::AIR.to_state(),
            block.default_state,
        ) {
            return None;
        }
        let advancements = player.advancements.try_lock().ok()?;
        if advancements.has_placement_listener(block.name) {
            return None;
        }
        let mut stats = player.stats.try_lock().ok()?;
        let mut next = held.clone();
        next.decrement_unless_creative(mode, 1);
        let equipment = inventory.entity_equipment.try_lock().ok()?;
        let packets = self.encode_personal_inventory(
            inventory,
            &equipment,
            main.iter()
                .enumerate()
                .map(|(index, stack)| if index == slot { &next } else { stack }),
            &cursor,
        )?;
        drop(equipment);
        self.commit_regional_main_hand(
            player,
            server,
            &held,
            &next,
            !held.are_equal(&next),
            || {
                place(state)?;
                main[slot] = next.clone();
                stats.increment(StatisticCategory::Used, i32::from(held.item.id), 1);
                Some(())
            },
        )?;
        drop(stats);
        drop(advancements);
        drop(main);
        for packet in packets {
            if !self.try_enqueue_packet_data_unobserved(packet) {
                break;
            }
        }
        Some(state)
    }
}
