use crate::block::{
    AttackArgs, BlockBehaviour, BlockMetadata, BlockPlacement, OnEntityStepArgs, RandomTickArgs,
    UseWithItemArgs, registry::BlockActionResult,
};
use crate::world::World;
use pumpkin_data::block_properties::RedstoneOreLikeProperties;
use pumpkin_data::data_component_impl::EquipmentSlot;
use pumpkin_data::{Block, BlockId};
use pumpkin_util::Hand;
use pumpkin_util::math::position::BlockPos;
use pumpkin_world::world::BlockFlags;
use std::sync::Arc;

pub struct RedstoneOreBlock;

impl BlockMetadata for RedstoneOreBlock {
    fn ids() -> Box<[BlockId]> {
        [BlockId::REDSTONE_ORE, BlockId::DEEPSLATE_REDSTONE_ORE].into()
    }
}

impl RedstoneOreBlock {
    fn light_up(world: &Arc<World>, pos: &BlockPos, block: &Block) {
        let state = world.get_block_state(pos);
        let mut props = RedstoneOreLikeProperties::from_state_id(state.id);
        if !props.lit {
            props.lit = true;
            world.set_block_state(pos, props.to_state_id(block), BlockFlags::NOTIFY_ALL);
        }
    }
}

impl BlockBehaviour for RedstoneOreBlock {
    fn attack(&self, args: AttackArgs<'_>) {
        Self::light_up(args.world, args.position, args.block);
    }

    fn use_with_item(&self, args: UseWithItemArgs<'_>) -> BlockActionResult {
        Self::light_up(args.world, args.position, args.block);
        if args.item_stack.is_empty() {
            return BlockActionResult::Success;
        }
        let Some(placed_block) = Block::from_item_id(args.item_stack.item.id) else {
            return BlockActionResult::Success;
        };
        // Vanilla only passes a block item on when BlockPlaceContext.canPlace() holds; otherwise the use is consumed.
        let placement = BlockPlacement {
            clicked_pos: *args.position,
            clicked_face: *args.hit.face,
            cursor_pos: *args.hit.cursor_pos,
            hand: if *args.equipment_slot == EquipmentSlot::OFF_HAND {
                Hand::Left
            } else {
                Hand::Right
            },
        };
        if args
            .server
            .block_registry
            .placement_target(args.world, args.player, placed_block, &placement)
            .is_some()
        {
            BlockActionResult::Pass
        } else {
            BlockActionResult::Success
        }
    }

    fn on_entity_step(&self, args: OnEntityStepArgs<'_>) {
        if !args.entity.is_stepping_carefully() {
            Self::light_up(args.world, args.position, args.block);
        }
    }

    fn random_tick(&self, args: RandomTickArgs<'_>) {
        let state = args.world.get_block_state(args.position);
        let mut props = RedstoneOreLikeProperties::from_state_id(state.id);

        if props.lit {
            props.lit = false;
            args.world.set_block_state(
                args.position,
                props.to_state_id(args.block),
                BlockFlags::NOTIFY_ALL,
            );
        }
    }
}
