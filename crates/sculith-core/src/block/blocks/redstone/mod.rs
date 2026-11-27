use pumpkin_data::{
    Block, BlockDirection, BlockState, BlockStateId, block_properties::RedstoneWireLikeProperties,
};
use sculith_util::math::position::BlockPos;

use crate::world::World;

pub mod bell;
pub mod buttons;
pub mod comparator;
pub mod copper_bulb;
pub mod crafter;
pub mod daylight_detector;
pub mod dropper;
pub mod lever;
pub mod lightning_rod;
pub mod observer;
pub mod pressure_plate;
pub mod rails;
pub mod redstone_block;
pub mod redstone_lamp;
pub mod redstone_ore;
pub mod redstone_torch;
pub mod redstone_wire;
pub mod repeater;
pub mod sculk_sensor;
pub mod target_block;
pub mod tripwire;
pub mod tripwire_hook;

// abstract
pub mod abstract_redstone_gate;
pub mod dispenser;

/// Strongest redstone signal (`Redstone.SIGNAL_MAX`).
pub const SIGNAL_MAX: u8 = 15;

const POWER_SOUND_DATA: i32 = -1;

const fn power_sound(powered: bool) -> &'static str {
    if powered { "power.on" } else { "power.off" }
}

fn mounted_neighbors_are_inert(
    position: BlockPos,
    face: BlockDirection,
    read: &dyn Fn(BlockPos) -> Option<BlockStateId>,
    inert: impl Fn(BlockStateId) -> bool,
) -> bool {
    let support = position.offset(face.opposite().to_offset());
    if !read(support).is_some_and(|state| inert(state) && state.to_state().is_side_solid(face)) {
        return false;
    }

    // Strong power at the support also notifies the blocks around it.
    [position, support].into_iter().all(|source| {
        BlockDirection::all().into_iter().all(|direction| {
            let neighbor = source.offset(direction.to_offset());
            neighbor == position || read(neighbor).is_some_and(&inert)
        })
    })
}

pub fn is_emitting_redstone_power(
    block: &Block,
    state: &BlockState,
    world: &World,
    pos: &BlockPos,
    facing: BlockDirection,
) -> bool {
    get_redstone_power(block, state, world, pos, facing) > 0
}

pub fn get_redstone_power(
    block: &Block,
    state: &BlockState,
    world: &World,
    pos: &BlockPos,
    facing: BlockDirection,
) -> u8 {
    if state.is_solid_block() {
        return std::cmp::max(
            get_max_strong_power(world, pos, true),
            get_weak_power(block, state, world, pos, facing, true),
        );
    }
    get_weak_power(block, state, world, pos, facing, true)
}

pub fn get_redstone_power_no_dust(
    block: &Block,
    state: &BlockState,
    world: &World,
    pos: BlockPos,
    facing: BlockDirection,
) -> u8 {
    if state.is_solid_block() {
        return std::cmp::max(
            get_max_strong_power(world, &pos, false),
            get_weak_power(block, state, world, &pos, facing, false),
        );
    }
    get_weak_power(block, state, world, &pos, facing, false)
}

pub fn get_max_strong_power(world: &World, pos: &BlockPos, dust_power: bool) -> u8 {
    let mut max_power = 0;
    for side in BlockDirection::all() {
        let (block, state) = world.get_block_and_state(&pos.offset(side.to_offset()));
        max_power = max_power.max(get_strong_power(
            block,
            state,
            world,
            &pos.offset(side.to_offset()),
            side,
            dust_power,
        ));
    }
    max_power
}

pub fn get_max_weak_power(world: &World, pos: &BlockPos, dust_power: bool) -> u8 {
    let mut max_power = 0;
    for side in BlockDirection::all() {
        let (block, state) = world.get_block_and_state(&pos.offset(side.to_offset()));
        max_power = max_power.max(get_weak_power(
            block,
            state,
            world,
            &pos.offset(side.to_offset()),
            side,
            dust_power,
        ));
    }
    max_power
}

fn get_weak_power(
    block: &Block,
    state: &BlockState,
    world: &World,
    pos: &BlockPos,
    side: BlockDirection,
    dust_power: bool,
) -> u8 {
    if !dust_power && block == &Block::REDSTONE_WIRE {
        return 0;
    }
    world
        .block_registry
        .get_weak_redstone_power(block, world, pos, state, side)
}

fn get_strong_power(
    block: &Block,
    state: &BlockState,
    world: &World,
    pos: &BlockPos,
    side: BlockDirection,
    dust_power: bool,
) -> u8 {
    if !dust_power && block == &Block::REDSTONE_WIRE {
        return 0;
    }
    world
        .block_registry
        .get_strong_redstone_power(block, world, pos, state, side)
}

pub fn block_receives_redstone_power(world: &World, pos: &BlockPos) -> bool {
    for facing in BlockDirection::all() {
        let neighbor_pos = pos.offset(facing.to_offset());
        let (block, state) = world.get_block_and_state(&neighbor_pos);
        if is_emitting_redstone_power(block, state, world, &neighbor_pos, facing) {
            return true;
        }
    }
    false
}

#[must_use]
pub fn is_diode(block: &Block) -> bool {
    block == &Block::REPEATER || block == &Block::COMPARATOR
}

pub fn diode_get_input_strength(world: &World, pos: &BlockPos, facing: BlockDirection) -> u8 {
    let input_pos = pos.offset(facing.to_offset());
    let (input_block, input_state) = world.get_block_and_state(&input_pos);
    let power: u8 = get_redstone_power(input_block, input_state, world, &input_pos, facing);
    if power >= 15 {
        return power;
    }
    power.max(if input_block == &Block::REDSTONE_WIRE {
        RedstoneWireLikeProperties::from_state_id(input_state.id).power
    } else {
        0
    })
}
