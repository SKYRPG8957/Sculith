use crate::block::{
    BlockBehaviour, GetStateForNeighborUpdateArgs, OnPlaceArgs, PathComputationType,
    is_exception_for_connection,
};
use crate::world::World;
use pumpkin_data::block_properties::HorizontalFacing;
use pumpkin_data::fluid::Fluid;
use pumpkin_data::tag::Taggable;
use pumpkin_data::{Block, BlockDirection, BlockState, BlockStateId, HorizontalFacingExt, tag};
use pumpkin_macros::pumpkin_block_from_tag;
use sculith_util::math::position::BlockPos;
use sculith_world::tick::TickPriority;

type FenceGateProperties = pumpkin_data::block_properties::OakFenceGateLikeProperties;
type FenceProperties = pumpkin_data::block_properties::OakFenceLikeProperties;

#[pumpkin_block_from_tag("minecraft:fences")]
pub struct FenceBlock;

impl BlockBehaviour for FenceBlock {
    fn on_place(&self, args: OnPlaceArgs<'_>) -> BlockStateId {
        let mut fence_props = FenceProperties::default(args.block);
        fence_props.waterlogged = args.replacing.water_source();

        compute_fence_state(fence_props, args.world, args.block, args.position)
    }

    fn get_state_for_neighbor_update(
        &self,
        args: GetStateForNeighborUpdateArgs<'_>,
    ) -> BlockStateId {
        let fence_props = FenceProperties::from_state_id(args.state_id);
        if fence_props.waterlogged {
            args.world.schedule_fluid_tick(
                &Fluid::WATER,
                *args.position,
                Fluid::WATER.flow_speed as u8,
                TickPriority::Normal,
            );
        }
        compute_fence_state(fence_props, args.world, args.block, args.position)
    }

    fn is_pathfindable(&self, _state: &BlockState, _computation_type: PathComputationType) -> bool {
        false
    }
}

pub fn compute_fence_state(
    fence_props: FenceProperties,
    world: &World,
    block: &Block,
    block_pos: &BlockPos,
) -> BlockStateId {
    let current = fence_props.to_state_id(block);
    connected_fence_state(fence_props, block, *block_pos, |pos| {
        Some(world.get_block_state_id(&pos))
    })
    .unwrap_or(current)
}

pub(crate) fn dry_fence_keeps_state(
    state: BlockStateId,
    position: BlockPos,
    read: impl Fn(BlockPos) -> Option<BlockStateId>,
) -> bool {
    let block = state.to_block();
    if !block.has_tag(&tag::Block::MINECRAFT_FENCES) {
        return false;
    }
    let properties = FenceProperties::from_state_id(state);
    !properties.waterlogged
        && connected_fence_state(properties, block, position, read) == Some(state)
}

fn connected_fence_state(
    mut fence_props: FenceProperties,
    block: &Block,
    block_pos: BlockPos,
    read: impl Fn(BlockPos) -> Option<BlockStateId>,
) -> Option<BlockStateId> {
    for direction in BlockDirection::horizontal() {
        let other_block_pos = block_pos.offset(direction.to_offset());
        let other = read(other_block_pos)?;

        let connected = connects_to(
            block,
            other.to_block(),
            other.to_state(),
            direction.to_block_direction(),
        );
        match direction {
            HorizontalFacing::North => fence_props.north = connected,
            HorizontalFacing::South => fence_props.south = connected,
            HorizontalFacing::West => fence_props.west = connected,
            HorizontalFacing::East => fence_props.east = connected,
        }
    }

    Some(fence_props.to_state_id(block))
}

fn is_same_fence(from: &Block, to: &Block) -> bool {
    to.has_tag(&tag::Block::MINECRAFT_FENCES)
        && (from.has_tag(&tag::Block::MINECRAFT_WOODEN_FENCES)
            == to.has_tag(&tag::Block::MINECRAFT_WOODEN_FENCES))
}

fn connects_to(from: &Block, to: &Block, to_state: &BlockState, direction: BlockDirection) -> bool {
    let same_fence = is_same_fence(from, to);
    let gate = to.has_tag(&tag::Block::MINECRAFT_FENCE_GATES) && {
        let fence_gate_props = FenceGateProperties::from_state_id(to_state.id);
        BlockDirection::from_cardinal_direction(fence_gate_props.facing).to_axis()
            == direction.rotate_clockwise().to_axis()
    };
    (!is_exception_for_connection(to) && to_state.is_side_solid(direction.opposite()))
        || same_fence
        || gate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::blocks::fence_gates::manual_fence_gate_toggle;

    #[test]
    fn regional_gate_toggle_preserves_current_dry_fence_connections() {
        let pos = BlockPos::new(0, 64, 0);
        let gate_pos = BlockPos::new(1, 64, 0);
        let mut gate = FenceGateProperties::default(&Block::OAK_FENCE_GATE);
        gate.facing = HorizontalFacing::North;
        let closed = gate.to_state_id(&Block::OAK_FENCE_GATE);
        let open =
            manual_fence_gate_toggle(closed, HorizontalFacing::South).map(|toggle| toggle.state);
        assert!(open.is_some());
        let after = |neighbor| {
            if neighbor == gate_pos {
                open
            } else {
                Some(BlockStateId::AIR)
            }
        };
        let mut fence = FenceProperties::default(&Block::OAK_FENCE);
        assert!(!dry_fence_keeps_state(
            fence.to_state_id(&Block::OAK_FENCE),
            pos,
            after
        ));
        fence.east = true;
        assert!(dry_fence_keeps_state(
            fence.to_state_id(&Block::OAK_FENCE),
            pos,
            after
        ));
        fence.waterlogged = true;
        assert!(!dry_fence_keeps_state(
            fence.to_state_id(&Block::OAK_FENCE),
            pos,
            after
        ));
    }
}
