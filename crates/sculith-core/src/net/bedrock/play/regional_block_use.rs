use std::sync::{Arc, atomic::Ordering::Relaxed};

use pumpkin_data::{
    Block, BlockDirection, BlockStateId,
    data_component_impl::{BlockStateImpl, BlockTransformerImpl},
    item::{Item, JavaToBedrockItemMapping},
    statistic::StatisticCategory,
};
use pumpkin_inventory::player::player_inventory::PlayerInventory;
use sculith_protocol::{
    RawPacket,
    bedrock::server::{
        inventory_transaction::{
            HandSlot, SInventoryTransaction, TransactionData, UseItemTransactionData,
        },
        player_auth_input::{InputData, PlayerInventoryAction, SPlayerAuthInput},
    },
    packet::Packet,
    serial::PacketRead,
};
use sculith_util::{
    GameMode,
    math::{position::BlockPos, vector2::Vector2},
};

use crate::{
    block::blocks::{
        BlockToggle,
        doors::{can_toggle_door, regional_door_toggle},
        fence_gates::manual_fence_gate_toggle,
        redstone::lever::LeverToggle,
        trapdoor::manual_trapdoor_toggle,
    },
    entity::{
        EntityBase,
        player::{Player, client_move::RegionalMoveStamp},
    },
    net::bedrock::BedrockClient,
    server::Server,
    world::World,
};

use super::{
    inventory_action::SIMULATION_TICK_TRIGGER, player_inventory::matches_item_type_and_count,
};

const REGIONAL_BLOCK_USE_BYTES: usize = 512;

enum RegionalBlockUse {
    Placed(BlockPos, BlockStateId),
    Activated(BlockPos, BlockStateId, RegionalActivation),
}

enum RegionalActivation {
    Opening(BlockToggle),
    Lever(LeverToggle),
}

impl RegionalActivation {
    fn commit(
        &self,
        world: &World,
        server: &Server,
        position: BlockPos,
        expected: BlockStateId,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<()> {
        match self {
            Self::Opening(toggle) => {
                world.try_toggle_regional_block(server, position, expected, toggle, owns)
            }
            Self::Lever(toggle) => {
                world.try_pull_regional_lever(server, position, expected, toggle, owns)
            }
        }
    }
}

impl RegionalBlockUse {
    fn publish(self, world: &World, player: &Player) {
        match self {
            Self::Placed(position, state) => {
                world.publish_regional_placement(position, state);
                tracing::trace!(player = %player.gameprofile.name, ?position, "Placed Bedrock block on its region");
            }
            Self::Activated(position, expected, RegionalActivation::Opening(toggle)) => {
                world.publish_regional_toggle(player, position, expected, &toggle);
                tracing::trace!(player = %player.gameprofile.name, ?position, paired = toggle.other_half.is_some(), "Toggled Bedrock block on its region");
            }
            Self::Activated(position, expected, RegionalActivation::Lever(toggle)) => {
                world.publish_regional_lever(position, expected, &toggle);
                tracing::trace!(player = %player.gameprofile.name, ?position, "Pulled Bedrock lever on its region");
            }
        }
    }
}

fn manual_toggle(player: &Player, state: BlockStateId) -> Option<BlockToggle> {
    manual_trapdoor_toggle(state)
        .or_else(|| manual_fence_gate_toggle(state, player.get_entity().get_horizontal_facing()))
}

impl BedrockClient {
    pub(in crate::net::bedrock) fn inline_block_use(
        input: &SPlayerAuthInput,
    ) -> Option<&UseItemTransactionData> {
        let interaction = input.item_interaction.as_ref()?;
        let click = &interaction.transaction;
        if !input
            .input_data
            .get(InputData::PerformItemInteraction as usize)
            || input.item_stack_request.is_some()
            || input.block_actions.is_some()
            || interaction.legacy_request_id.0 != 0
            || !interaction.legacy_slots.is_empty()
            || !interaction.actions.is_empty()
            || click.action_type.0 != 0
            || click.hand != HandSlot::Mainhand
        {
            return None;
        }
        Some(click)
    }

    pub(in crate::net::bedrock) fn supports_inline_block_use(
        player: &Player,
        data: &UseItemTransactionData,
    ) -> bool {
        // Admit existing placement candidates and known activations. The commit
        // checks the block and the server's selected stack after movement.
        let item = &data.item_in_hand;
        if i16::try_from(item.id.0)
            .ok()
            .and_then(|id| JavaToBedrockItemMapping::from_bedrock(id, item.aux_value.0))
            .and_then(|mapping| Block::from_item_id(mapping.java_item.id))
            .is_some()
        {
            return true;
        }
        player
            .world()
            .level
            .try_read_block_state(data.block_position)
            .is_some_and(|state| {
                state.to_block() == &Block::LEVER
                    || can_toggle_door(state.to_block())
                    || manual_toggle(player, state).is_some()
            })
    }

    pub(in crate::net::bedrock) fn finish_inline_block_use(
        &self,
        player: &Arc<Player>,
        interaction: PlayerInventoryAction,
        stamp: RegionalMoveStamp,
        movement_finished: bool,
    ) {
        if !player.has_client_loaded() || !player.has_stable_world_membership() {
            return;
        }
        let world = player.world();
        let packet = SInventoryTransaction::from(interaction);
        if movement_finished
            && stamp.is_current(player, &world)
            && !player.living_entity.is_dead_or_dying()
        {
            self.handle_inventory_action(player, packet);
        } else {
            if stamp.matches_world(player, &world) {
                Self::correct_transaction_block_prediction(player, &packet.transaction_data);
            }
            self.resend_player_inventory(player);
        }
    }

    pub(in crate::net::bedrock) fn can_route_block_use(packet: &RawPacket) -> bool {
        packet.id == SInventoryTransaction::PACKET_ID
            && packet.payload.len() <= REGIONAL_BLOCK_USE_BYTES
            // No legacy request, legacy slots or inventory actions. The
            // transaction is UseItem, with the click-block action.
            && packet.payload.starts_with(&[0, 0, 2, 0, 0])
    }

    pub(in crate::net::bedrock) fn try_regional_block_use(
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
        self.try_apply_regional_block_use(player, server, &data, owns)
    }

    pub(in crate::net::bedrock) fn try_apply_regional_block_use(
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
            || !matches!(
                mode,
                GameMode::Survival | GameMode::Creative | GameMode::Adventure
            )
            || self.inventory_opened.load(Relaxed)
            || server.stop_token.is_cancelled()
            || server.tick_rate_manager.is_frozen()
        {
            return None;
        }
        let world = player.world();
        let entity = player.get_entity();
        let action = {
            let _edit = player.try_regional_inventory_edit()?;
            let teleport = player.awaiting_teleport.try_lock().ok()?;
            if teleport.is_some() {
                return None;
            }
            entity.bounding_box.try_with_current_world(&world, || {
                let clicked = data.block_position;
                let eligible = || {
                    !self.is_closed()
                        && player.can_simulate_in(&world)
                        && player.has_stable_world_membership()
                        && player.has_client_loaded()
                        && !player.living_entity.is_dead_or_dying()
                        && player.gamemode.load() == mode
                        && usize::from(player.inventory.get_selected_slot()) == slot
                        && owns(entity.chunk_pos.load())
                        && owns(clicked.chunk_position())
                };
                if !eligible() || !player.try_bedrock_block_reach(&clicked, mode, 1.0)? {
                    return None;
                }
                let state = world.level.try_read_block_state(clicked)?;
                let activation = if can_toggle_door(state.to_block()) {
                    Some(RegionalActivation::Opening(regional_door_toggle(
                        clicked,
                        state,
                        |pos| world.level.try_read_block_state(pos),
                    )?))
                } else {
                    LeverToggle::from_state(state)
                        .map(RegionalActivation::Lever)
                        .or_else(|| manual_toggle(player, state).map(RegionalActivation::Opening))
                };
                if let Some(activation) = activation {
                    let main = player.inventory.main_inventory.try_read().ok()?;
                    let held = main.get(slot)?;
                    if (entity.is_sneaking() && !held.is_empty())
                        || (held.item.id == Item::FISHING_ROD.id
                            && data.trigger_type == SIMULATION_TICK_TRIGGER)
                        || !matches_item_type_and_count(&data.item_in_hand, held)
                    {
                        return None;
                    }
                    activation.commit(&world, server, clicked, state, &|chunk| {
                        owns(chunk) && eligible()
                    })?;
                    return Some(RegionalBlockUse::Activated(clicked, state, activation));
                }
                if !matches!(mode, GameMode::Survival | GameMode::Creative)
                    || !self.inventory_request_can_run_on_region(player, server)
                {
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
                Some(RegionalBlockUse::Placed(position, state))
            })??
        };
        action.publish(&world, player);
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
