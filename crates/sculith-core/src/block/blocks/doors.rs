use pumpkin_data::BlockDirection;
use pumpkin_data::BlockStateId;
use pumpkin_data::block_properties::Axis;
use pumpkin_data::block_properties::DoorHinge;
use pumpkin_data::block_properties::DoubleBlockHalf;
use pumpkin_data::block_properties::HorizontalFacing;
use pumpkin_data::sound::Sound;
use pumpkin_data::tag::Taggable;
use pumpkin_data::{Block, BlockState, tag};
use pumpkin_macros::pumpkin_block_from_tag;
use sculith_util::math::position::BlockPos;
use sculith_util::math::vector3::Vector3;
use sculith_world::world::BlockAccessor;
use sculith_world::world::BlockFlags;
use std::sync::Arc;

use crate::block::BlockBehaviour;
use crate::block::BrokenArgs;
use crate::block::CanPlaceAtArgs;
use crate::block::GetStateForNeighborUpdateArgs;
use crate::block::NormalUseArgs;
use crate::block::OnNeighborUpdateArgs;
use crate::block::OnPlaceArgs;
use crate::block::OnStateReplacedArgs;
use crate::block::PathComputationType;
use crate::block::PlacedArgs;
use crate::block::blocks::redstone::block_receives_redstone_power;
use crate::block::registry::BlockActionResult;
use crate::world::World;
use sculith_util::GameMode;

use super::BlockToggle;

type DoorProperties = pumpkin_data::block_properties::OakDoorLikeProperties;

fn other_half(position: BlockPos, half: DoubleBlockHalf) -> BlockPos {
    match half {
        DoubleBlockHalf::Upper => position.down(),
        DoubleBlockHalf::Lower => position.up(),
    }
}

pub(crate) fn door_companion(position: BlockPos, state: BlockStateId) -> Option<BlockPos> {
    state
        .to_block()
        .has_tag(&tag::Block::MINECRAFT_DOORS)
        .then(|| other_half(position, DoorProperties::from_state_id(state).half))
}

fn door_update(
    position: BlockPos,
    block: &Block,
    next: DoorProperties,
    read: impl Fn(BlockPos) -> Option<BlockStateId>,
) -> BlockToggle {
    let toggle = BlockToggle::new(
        next.to_state_id(block),
        next.open,
        get_sound(block, next.open),
        ("door.open", "door.close"),
    );
    let other_position = other_half(position, next.half);
    let Some(other) = read(other_position).filter(|other| other.to_block() == block) else {
        return toggle;
    };
    let half = DoorProperties::from_state_id(other).half;
    if half == next.half {
        return toggle;
    }
    let other_next = DoorProperties { half, ..next }.to_state_id(block);
    toggle.with_other_half(other_position, other, other_next)
}

pub(crate) fn can_toggle_door(block: &Block) -> bool {
    block.has_tag(&tag::Block::MINECRAFT_DOORS) && block != &Block::IRON_DOOR
}

fn manual_door_toggle(
    position: BlockPos,
    state: BlockStateId,
    read: impl Fn(BlockPos) -> Option<BlockStateId>,
) -> Option<BlockToggle> {
    let block = state.to_block();
    if !can_toggle_door(block) {
        return None;
    }
    let mut next = DoorProperties::from_state_id(state);
    next.open = !next.open;
    Some(door_update(position, block, next, read))
}

pub(crate) fn regional_door_toggle(
    position: BlockPos,
    state: BlockStateId,
    read: impl Fn(BlockPos) -> Option<BlockStateId>,
) -> Option<BlockToggle> {
    let toggle = manual_door_toggle(position, state, read)?;
    let (_, companion, _) = toggle.other_half?;
    let previous = DoorProperties::from_state_id(state);
    let companion_properties = DoorProperties {
        half: DoorProperties::from_state_id(companion).half,
        ..previous
    };
    (!previous.powered && companion_properties.to_state_id(state.to_block()) == companion)
        .then_some(toggle)
}

// Todo: The sounds should be from BlockSetType
fn get_sound(block: &Block, open: bool) -> Sound {
    if open {
        if block.has_tag(&tag::Block::MINECRAFT_WOODEN_DOORS) {
            Sound::BlockWoodenDoorOpen
        } else if block == &Block::IRON_DOOR {
            Sound::BlockIronDoorOpen
        } else {
            Sound::BlockCopperDoorOpen
        }
    } else if block.has_tag(&tag::Block::MINECRAFT_WOODEN_DOORS) {
        Sound::BlockWoodenDoorClose
    } else if block == &Block::IRON_DOOR {
        Sound::BlockIronDoorClose
    } else {
        Sound::BlockCopperDoorClose
    }
}

#[expect(clippy::pedantic)]
#[inline]
fn get_hinge(
    world: &World,
    pos: &BlockPos,
    hit: Vector3<f32>,
    facing: HorizontalFacing,
) -> DoorHinge {
    let top_pos = pos.up();
    let left_dir = facing.rotate_counter_clockwise();
    let left_pos = pos.offset(left_dir.to_offset());
    let (_left_block, left_state) = world.get_block_and_state(&left_pos);
    let top_left = top_pos.offset(left_dir.to_offset());
    let top_state = world.get_block_state(&top_left);
    let right_dir = facing.rotate_clockwise();
    let right_pos = pos.offset(right_dir.to_offset());
    let (_right_block, right_state) = world.get_block_and_state(&right_pos);
    let top_right = top_pos.offset(right_dir.to_offset());
    let top_right_state = world.get_block_state(&top_right);

    let has_left_door = world
        .get_block(&left_pos)
        .has_tag(&tag::Block::MINECRAFT_DOORS)
        && DoorProperties::from_state_id(left_state.id).half == DoubleBlockHalf::Lower;

    let has_right_door = world
        .get_block(&right_pos)
        .has_tag(&tag::Block::MINECRAFT_DOORS)
        && DoorProperties::from_state_id(right_state.id).half == DoubleBlockHalf::Lower;

    let score = -(left_state.is_full_cube() as i32) - (top_state.is_full_cube() as i32)
        + right_state.is_full_cube() as i32
        + top_right_state.is_full_cube() as i32;

    if (!has_left_door || has_right_door) && score <= 0 {
        if (!has_right_door || has_left_door) && score >= 0 {
            let offset = facing.to_offset();
            if (offset.x >= 0 || hit.z > 0.5)
                && (offset.x <= 0 || hit.z < 0.5)
                && (offset.z >= 0 || hit.x < 0.5)
                && (offset.z <= 0 || hit.x > 0.5)
            {
                DoorHinge::Left
            } else {
                DoorHinge::Right
            }
        } else {
            DoorHinge::Left
        }
    } else {
        DoorHinge::Right
    }
}

#[pumpkin_block_from_tag("minecraft:doors")]
pub struct DoorBlock;

impl DoorBlock {
    #[must_use]
    pub fn is_wooden_door(world: &World, block_pos: &BlockPos) -> bool {
        let block = world.get_block(block_pos);
        block.has_tag(&tag::Block::MINECRAFT_WOODEN_DOORS)
    }

    #[must_use]
    pub fn is_open(world: &World, block_pos: &BlockPos) -> bool {
        let (block, block_state) = world.get_block_and_state_id(block_pos);
        if !block.has_tag(&tag::Block::MINECRAFT_DOORS) {
            return false;
        }
        let door_props = DoorProperties::from_state_id(block_state);
        door_props.open
    }

    pub fn set_open(world: &Arc<World>, block_pos: &BlockPos, open: bool) {
        BlockToggle::apply_in_world(None, world, *block_pos, |state| {
            let block = state.to_block();
            if !block.has_tag(&tag::Block::MINECRAFT_DOORS) {
                return None;
            }
            let mut next = DoorProperties::from_state_id(state);
            if next.open == open {
                return None;
            }
            next.open = open;
            Some(door_update(*block_pos, block, next, |pos| {
                Some(world.get_block_state_id(&pos))
            }))
        });
    }
}

impl BlockBehaviour for DoorBlock {
    fn on_place(&self, args: OnPlaceArgs<'_>) -> BlockStateId {
        let mut door_props = DoorProperties::default(args.block);
        let facing = args.player.living_entity.entity.get_horizontal_facing();
        let powered = block_receives_redstone_power(args.world, args.position)
            || block_receives_redstone_power(args.world, &args.position.up());
        door_props.facing = facing;
        door_props.half = DoubleBlockHalf::Lower;
        door_props.hinge = get_hinge(args.world, args.position, args.placement.cursor_pos, facing);
        door_props.open = powered;
        door_props.powered = powered;
        door_props.to_state_id(args.block)
    }

    fn can_place_at(&self, args: CanPlaceAtArgs<'_>) -> bool {
        has_support(args.block_accessor, args.position)
            && args
                .block_accessor
                .get_block_state(&args.position.up())
                .replaceable()
    }

    fn placed(&self, args: PlacedArgs<'_>) {
        {
            let mut door_props = DoorProperties::from_state_id(args.state_id);
            door_props.half = DoubleBlockHalf::Upper;

            args.world.set_block_state(
                &args.position.offset(BlockDirection::Up.to_offset()),
                door_props.to_state_id(args.block),
                BlockFlags::NOTIFY_ALL | BlockFlags::SKIP_BLOCK_ADDED_CALLBACK,
            );
        }
    }

    fn normal_use(&self, args: NormalUseArgs<'_>) -> BlockActionResult {
        if BlockToggle::apply_in_world(Some(args.player), args.world, *args.position, |state| {
            manual_door_toggle(*args.position, state, |pos| {
                Some(args.world.get_block_state_id(&pos))
            })
        })
        .is_some()
        {
            BlockActionResult::Success
        } else {
            BlockActionResult::Pass
        }
    }

    fn broken(&self, args: BrokenArgs<'_>) {
        let door_props = DoorProperties::from_state_id(args.state.id);
        let other_half_pos = match door_props.half {
            DoubleBlockHalf::Upper => args.position.down(),
            DoubleBlockHalf::Lower => args.position.up(),
        };

        let (other_block, other_state) = args.world.get_block_and_state(&other_half_pos);
        if other_block.id != args.block.id {
            args.world.update_neighbors(&other_half_pos, None);
            return; // Neighbor is already gone or is a different block
        }

        let other_props = DoorProperties::from_state_id(other_state.id);
        if other_props.half == door_props.half {
            return;
        }

        let is_creative = args.player.gamemode.load() == GameMode::Creative;
        let flags = if door_props.half == DoubleBlockHalf::Upper && !is_creative {
            BlockFlags::NOTIFY_ALL
        } else {
            BlockFlags::SKIP_DROPS | BlockFlags::NOTIFY_ALL
        };

        args.world
            .break_block(&other_half_pos, Some(args.player), flags);
    }

    fn on_neighbor_update(&self, args: OnNeighborUpdateArgs<'_>) {
        let position = *args.position;
        BlockToggle::apply_world_update(
            None,
            args.world,
            position,
            BlockFlags::NOTIFY_ALL,
            |state| {
                if state.to_block() != args.block {
                    return None;
                }
                let previous = DoorProperties::from_state_id(state);
                let other = other_half(position, previous.half);
                let powered = block_receives_redstone_power(args.world, &position)
                    || block_receives_redstone_power(args.world, &other);
                if powered == previous.powered {
                    return None;
                }
                let next = DoorProperties {
                    open: powered,
                    powered,
                    ..previous
                };
                let toggle = door_update(position, args.block, next, |pos| {
                    Some(args.world.get_block_state_id(&pos))
                });
                toggle
                    .other_half
                    .is_some()
                    .then_some((toggle, previous.open != powered))
            },
        );
    }

    fn get_state_for_neighbor_update(
        &self,
        args: GetStateForNeighborUpdateArgs<'_>,
    ) -> BlockStateId {
        let lv = DoorProperties::from_state_id(args.state_id).half;
        if args.direction.to_axis() != Axis::Y
            || (lv == DoubleBlockHalf::Lower) != (args.direction == BlockDirection::Up)
        {
            if lv == DoubleBlockHalf::Lower
                && args.direction == BlockDirection::Down
                && !has_support(args.world, args.position)
            {
                return BlockStateId::AIR;
            }
        } else if Block::from_state_id(args.neighbor_state_id).id == args.block.id
            && DoorProperties::from_state_id(args.neighbor_state_id).half != lv
        {
            let mut new_state = DoorProperties::from_state_id(args.neighbor_state_id);
            new_state.half = lv;
            return new_state.to_state_id(args.block);
        } else {
            return BlockStateId::AIR;
        }
        args.state_id
    }

    fn on_state_replaced(&self, args: OnStateReplacedArgs<'_>) {
        if args.moved {
            return;
        }

        let door_props = DoorProperties::from_state_id(args.old_state_id);
        let other_half_pos = match door_props.half {
            DoubleBlockHalf::Upper => args.position.down(),
            DoubleBlockHalf::Lower => args.position.up(),
        };

        let (other_block, other_state) = args.world.get_block_and_state(&other_half_pos);
        if other_block.id == args.block.id {
            let other_props = DoorProperties::from_state_id(other_state.id);
            if other_props.half != door_props.half {
                args.world.break_block(
                    &other_half_pos,
                    None,
                    BlockFlags::SKIP_DROPS | BlockFlags::NOTIFY_ALL,
                );
            }
        }
    }

    fn is_pathfindable(&self, state: &BlockState, computation_type: PathComputationType) -> bool {
        match computation_type {
            PathComputationType::Land | PathComputationType::Air => {
                DoorProperties::from_state_id(state.id).open
            }
            PathComputationType::Water => false,
        }
    }
}

fn has_support(world: &dyn BlockAccessor, block_pos: &BlockPos) -> bool {
    world
        .get_block_state(&block_pos.down())
        .is_side_solid(BlockDirection::Up)
}
