#[allow(clippy::wildcard_imports)]
use super::*;
use crate::block::BlockPlacement;
use crate::entity::player::ItemUseSnapshot;
use crate::item::registry::should_try_block_placement;
use pumpkin_data::block_properties::BlockProperties;

impl JavaClient {
    #[allow(clippy::too_many_lines)]
    pub fn handle_use_item_on(
        &self,
        player: &Arc<Player>,
        use_item_on: &SUseItemOn,
        server: &Arc<Server>,
    ) -> Result<(), BlockPlacingError> {
        if !player.has_client_loaded() {
            return Ok(());
        }
        player.update_last_action_time();
        self.update_sequence(use_item_on.sequence.0);

        let position = use_item_on.position;
        let cursor_pos = use_item_on.cursor_pos;

        let mut should_try_decrement = false;

        if !player.can_interact_with_block_at(&position, 1.0) {
            // TODO: maybe log?
            return Err(BlockPlacingError::BlockOutOfReach);
        }

        let Ok(face) = BlockDirection::try_from(use_item_on.face.0) else {
            return Err(BlockPlacingError::InvalidBlockFace);
        };

        let Ok(hand) = Hand::from_packet_id(use_item_on.hand.0) else {
            return Err(BlockPlacingError::InvalidHand);
        };

        let placement = BlockPlacement {
            clicked_pos: position,
            clicked_face: face,
            cursor_pos,
            hand,
        };
        let snapshot = ItemUseSnapshot::new(player, hand);
        let spectator = player.gamemode.load() == GameMode::Spectator;
        if !spectator {
            player.increment_stat(
                StatisticCategory::Used,
                i32::from(snapshot.stack.item.id),
                1,
            );
        }
        let Some(block) = snapshot.block_interaction(player, server, &placement) else {
            return Ok(());
        };
        let world = snapshot.world();
        if spectator {
            if let Some(factory) = server
                .block_registry
                .get_screen_handler_factory(block, player, &position, server, world)
            {
                player.open_handled_screen(factory.as_ref(), Some(position));
            }
            return Ok(());
        }

        let inventory = player.inventory();
        let before = &snapshot.stack;
        let mut item = before.clone();
        let slot_index = if matches!(hand, Hand::Right) {
            inventory.get_selected_slot() as usize
        } else {
            PlayerInventory::OFF_HAND_SLOT
        };

        let sneaking = player.get_entity().is_sneaking();

        // Code based on the java class ServerPlayerInteractionManager
        if !(sneaking
            && (!inventory.held_item().is_empty() || !inventory.off_hand_item().is_empty()))
        {
            let result = server
                .block_registry
                .use_clicked_block(player, server, world, block, &placement, &mut item);
            if result.consumes_action() {
                trigger_item_used_on_block_advancement(player, world, &position, block, before);
                sync_used_hand_item(player, hand, slot_index, before, item);

                if matches!(result, BlockActionResult::SuccessServer) {
                    player.swing_hand(hand, true);
                }
                return Ok(());
            }
        }

        if item.is_empty() {
            // TODO item cool down
            // If the hand is empty we stop here
            return Ok(());
        }

        let item_result = server
            .item_registry
            .use_on_block(&mut item, player, position, face, cursor_pos, block, server);

        if should_try_block_placement(&item_result) {
            // Check if the item is a block, because not every item can be placed :D
            let item_id = item.item.id;
            if let Some(block) = Block::from_item_id(item_id) {
                should_try_decrement = Self::run_is_block_place(player, block, server, &placement)?;
            }
        }

        if should_try_decrement {
            // TODO: Config
            // Decrease block count
            if player.gamemode.load() != GameMode::Creative {
                item.decrement(1);
            }
        }

        if item_result.consumes_action() || !item.are_equal(before) {
            trigger_item_used_on_block_advancement(player, world, &position, block, before);
        }

        if matches!(item_result, BlockActionResult::SuccessServer) {
            player.swing_hand(hand, true);
        }

        sync_used_hand_item(player, hand, slot_index, before, item);

        Ok(())
    }

    fn run_is_block_place(
        player: &Arc<Player>,
        block: &'static Block,
        server: &Arc<Server>,
        placement: &BlockPlacement,
    ) -> Result<bool, BlockPlacingError> {
        match server
            .block_registry
            .place_block(player, block, server, placement)
        {
            Ok(Some((final_block_pos, new_state))) => {
                player.try_send_client_packet(&CBlockUpdate::new(
                    final_block_pos,
                    VarInt(i32::from(new_state.as_u16())),
                ));
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(crate::block::registry::BlockPlacingError::InvalidGamemode) => {
                Err(BlockPlacingError::InvalidGamemode)
            }
            Err(crate::block::registry::BlockPlacingError::BlockOutOfWorld) => {
                Err(BlockPlacingError::BlockOutOfWorld)
            }
        }
    }
}

fn trigger_item_used_on_block_advancement(
    player: &Player,
    world: &World,
    position: &BlockPos,
    block: &Block,
    used_item: &ItemStack,
) {
    let state = pumpkin_data::block_properties::CopperBulbLikeProperties::handles_block_id(
        block.id,
    )
    .then(|| {
        let props = pumpkin_data::block_properties::CopperBulbLikeProperties::from_state_id(
            world.get_block_state(position).id,
        );
        ("lit".to_string(), props.r#lit.to_string())
    });
    let is_smokey = crate::block::blocks::campfire::CampfireBlock::is_smokey_pos(world, position);
    let biome = Some(world.get_biome(position).registry_id.to_string());

    player.trigger_advancement(
        crate::entity::player::advancement::trigger::AdvancementTrigger::ItemUsedOnBlock {
            block_id: format!("minecraft:{}", block.name),
            item_id: used_item.item.registry_key.to_string(),
            state,
            smokey: Some(is_smokey),
            biome,
        },
    );
}

fn sync_used_hand_item(
    player: &Player,
    hand: Hand,
    slot_index: usize,
    before: &ItemStack,
    after: ItemStack,
) {
    if after.are_equal(before) {
        return;
    }

    // Broadcast the break entity status before the slot sync; the client
    // needs the old item texture in the slot for break particles.
    if before.is_damageable() && after.is_empty() {
        let slot = if slot_index == player.inventory.get_selected_slot() as usize {
            &EquipmentSlot::MAIN_HAND
        } else {
            &EquipmentSlot::OFF_HAND
        };
        player.increment_stat(StatisticCategory::Broken, i32::from(before.item.id), 1);
        player.world().send_entity_status(
            player.get_entity(),
            equipment_break_status(slot),
            ActorEventID::None,
        );
    }

    player.inventory().set_stack_in_hand(hand, after.clone());
    player.sync_hand_slot(slot_index, after);
}
