use std::sync::Arc;

use crate::block::blocks::redstone::block_receives_redstone_power;
use crate::block::registry::BlockActionResult;
use crate::block::{
    BlockBehaviour, GetStateForNeighborUpdateArgs, NormalUseArgs, OnNeighborUpdateArgs,
    OnPlaceArgs, PathComputationType,
};
use crate::entity::EntityBase;
use crate::entity::player::Player;
use crate::world::World;
use pumpkin_data::block_properties::HorizontalFacing;
use pumpkin_data::sound::{Sound, SoundCategory};
use pumpkin_data::tag;
use pumpkin_data::tag::Taggable;
use pumpkin_data::{Block, BlockState, BlockStateId};
use pumpkin_macros::pumpkin_block_from_tag;
use sculith_util::math::position::BlockPos;
use sculith_world::world::BlockFlags;

use super::BlockToggle;

type FenceGateProperties = pumpkin_data::block_properties::OakFenceGateLikeProperties;

fn get_sound(block: &Block, open: bool) -> Sound {
    match (block, open) {
        (b, true) if b == &Block::BAMBOO_FENCE_GATE => Sound::BlockBambooWoodFenceGateOpen,
        (b, false) if b == &Block::BAMBOO_FENCE_GATE => Sound::BlockBambooWoodFenceGateClose,
        (b, true) if b == &Block::CHERRY_FENCE_GATE => Sound::BlockCherryWoodFenceGateOpen,
        (b, false) if b == &Block::CHERRY_FENCE_GATE => Sound::BlockCherryWoodFenceGateClose,
        (b, true) if b == &Block::CRIMSON_FENCE_GATE || b == &Block::WARPED_FENCE_GATE => {
            Sound::BlockNetherWoodFenceGateOpen
        }
        (b, false) if b == &Block::CRIMSON_FENCE_GATE || b == &Block::WARPED_FENCE_GATE => {
            Sound::BlockNetherWoodFenceGateClose
        }
        (_, true) => Sound::BlockFenceGateOpen,
        (_, false) => Sound::BlockFenceGateClose,
    }
}

pub fn toggle_fence_gate(
    world: &Arc<World>,
    block_pos: &BlockPos,
    player: &Player,
) -> BlockStateId {
    let facing = player.get_entity().get_horizontal_facing();
    BlockToggle::apply_in_world(Some(player), world, *block_pos, |state| {
        manual_fence_gate_toggle(state, facing)
    })
    .unwrap_or_else(|| world.get_block_state_id(block_pos))
}

pub(crate) fn manual_fence_gate_toggle(
    state: BlockStateId,
    facing: HorizontalFacing,
) -> Option<BlockToggle> {
    let block = state.to_block();
    if !block.has_tag(&tag::Block::MINECRAFT_FENCE_GATES) {
        return None;
    }
    let mut properties = FenceGateProperties::from_state_id(state);
    properties.open = !properties.open;
    if properties.open && properties.facing == facing.opposite() {
        properties.facing = facing;
    }
    Some(BlockToggle::new(
        properties.to_state_id(block),
        properties.open,
        get_sound(block, properties.open),
        ("fence_gate.open", "fence_gate.close"),
    ))
}

#[pumpkin_block_from_tag("minecraft:fence_gates")]
pub struct FenceGateBlock;

impl BlockBehaviour for FenceGateBlock {
    fn on_place(&self, args: OnPlaceArgs<'_>) -> BlockStateId {
        let mut fence_gate_props = FenceGateProperties::default(args.block);
        fence_gate_props.facing = args.player.get_entity().get_horizontal_facing();

        let powered = block_receives_redstone_power(args.world, args.position);
        fence_gate_props.powered = powered;
        fence_gate_props.open = powered;
        fence_gate_props.in_wall = is_in_wall(args.world, args.position, fence_gate_props.facing);

        fence_gate_props.to_state_id(args.block)
    }

    fn get_state_for_neighbor_update(
        &self,
        args: GetStateForNeighborUpdateArgs<'_>,
    ) -> BlockStateId {
        let mut fence_props = FenceGateProperties::from_state_id(args.state_id);
        let side_offset_left = args
            .position
            .offset(fence_props.facing.rotate_clockwise().to_offset());
        let side_offset_right = args
            .position
            .offset(fence_props.facing.rotate_counter_clockwise().to_offset());
        if args.neighbor_position == &side_offset_left
            || args.neighbor_position == &side_offset_right
        {
            fence_props.in_wall = is_in_wall(args.world, args.position, fence_props.facing);
        }
        fence_props.to_state_id(args.block)
    }

    fn normal_use(&self, args: NormalUseArgs<'_>) -> BlockActionResult {
        {
            toggle_fence_gate(args.world, args.position, args.player);

            BlockActionResult::Success
        }
    }

    fn on_neighbor_update(&self, args: OnNeighborUpdateArgs<'_>) {
        {
            let block_state = args.world.get_block_state(args.position);
            let mut fence_gate_props = FenceGateProperties::from_state_id(block_state.id);
            let powered = block_receives_redstone_power(args.world, args.position);

            if powered == fence_gate_props.powered {
                return;
            }

            fence_gate_props.powered = powered;

            if powered != fence_gate_props.open {
                fence_gate_props.open = powered;

                args.world.play_block_sound(
                    get_sound(args.block, powered),
                    SoundCategory::Blocks,
                    *args.position,
                );
                let game_event = if powered {
                    pumpkin_data::game_event::GameEvent::BlockOpen
                } else {
                    pumpkin_data::game_event::GameEvent::BlockClose
                };
                args.world
                    .emit_game_event(game_event.name(), args.position.to_centered_f64());
            }

            args.world.set_block_state(
                args.position,
                fence_gate_props.to_state_id(args.block),
                BlockFlags::NOTIFY_LISTENERS,
            );
        }
    }

    fn is_pathfindable(&self, state: &BlockState, computation_type: PathComputationType) -> bool {
        match computation_type {
            PathComputationType::Land | PathComputationType::Air => {
                FenceGateProperties::from_state_id(state.id).open
            }
            PathComputationType::Water => false,
        }
    }
}

fn is_in_wall(world: &World, pos: &BlockPos, facing: HorizontalFacing) -> bool {
    let side_offset_left = pos.offset(facing.rotate_clockwise().to_offset());
    let side_offset_right = pos.offset(facing.rotate_counter_clockwise().to_offset());

    world
        .get_block(&side_offset_left)
        .has_tag(&tag::Block::MINECRAFT_WALLS)
        || world
            .get_block(&side_offset_right)
            .has_tag(&tag::Block::MINECRAFT_WALLS)
}
