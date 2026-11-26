use super::{
    flowing_trait::{FlowingFluid, isolated_falling_plan, regional_fluid_neighbor_is_inert},
    pathfinder, physics,
};
use crate::{
    block::{FluidMetadata, fluid::FluidBehaviour},
    entity::EntityBase,
    world::World,
};
use pumpkin_data::{
    BlockDirection, BlockStateId, HorizontalFacingExt,
    fluid::{Fluid, FluidProperties, Level},
};
use pumpkin_world::{chunk::RegionalWaterTick, tick::TickPriority};
use sculith_util::math::position::BlockPos;
use std::sync::Arc;

pub struct FlowingWater;

impl FluidMetadata for FlowingWater {
    fn ids() -> Box<[u16]> {
        [Fluid::FLOWING_WATER.id].into()
    }
}

const WATER_FLOW_SPEED: u8 = 5;

impl FlowingWater {
    pub(crate) fn regional_water_tick(
        world: &World,
        position: BlockPos,
        state: BlockStateId,
        read: &dyn Fn(BlockPos) -> Option<BlockStateId>,
    ) -> Option<RegionalWaterTick> {
        let read = |at| world.is_in_build_limit(at).then(|| read(at)).flatten();
        let water = &Fluid::FLOWING_WATER;
        let source = Self.get_source(water, false).to_state_id(water);
        let falling = Self.get_flowing(water, Level::L8, true).to_state_id(water);
        if state != source
            && (state != falling || ![source, falling].contains(&read(position.up())?))
        {
            return None;
        }

        let below = position.down();
        let destination = read(below)?;
        if !physics::can_be_replaced(destination.to_state(), destination.to_block(), water) {
            return (state == source)
                .then(|| Self::regional_horizontal_tick(world, position, &read))
                .flatten();
        }
        isolated_falling_plan(world, position, water, falling, WATER_FLOW_SPEED, &read)
    }

    fn regional_horizontal_tick(
        world: &World,
        position: BlockPos,
        read: &dyn Fn(BlockPos) -> Option<BlockStateId>,
    ) -> Option<RegionalWaterTick> {
        let water = &Fluid::FLOWING_WATER;
        let dry_open = |at| {
            if at == position {
                return Some(false);
            }
            let state = read(at)?;
            if state == BlockStateId::AIR {
                Some(true)
            } else if physics::can_be_replaced(state.to_state(), state.to_block(), water) {
                // Replacing plants or other fluids requires the ordinary world callbacks.
                None
            } else {
                Some(false)
            }
        };
        let hole_below = |at: BlockPos| dry_open(at.down());
        let mut minimum = pathfinder::NO_REACHABLE_HOLE;
        let mut sides = [None; 4];
        for (index, direction) in BlockDirection::horizontal().into_iter().enumerate() {
            let target = position.offset(direction.to_offset());
            if !dry_open(target)? {
                continue;
            }
            if target.chunk_position() != position.chunk_position() {
                return None;
            }
            for nearby in BlockDirection::all() {
                let neighbor = target.offset(nearby.to_offset());
                if neighbor != position && !regional_fluid_neighbor_is_inert(world, read(neighbor)?)
                {
                    return None;
                }
            }
            let distance = if hole_below(target)? {
                0
            } else {
                pathfinder::slope_distance(
                    Self.get_max_flow_distance(world),
                    target,
                    direction.to_block_direction().opposite(),
                    hole_below,
                    dry_open,
                )?
            };
            if distance < minimum {
                sides.fill(None);
                minimum = distance;
            }
            if distance == minimum {
                sides[index] = Some(direction);
            }
        }
        if sides.iter().all(Option::is_none) {
            return Some(RegionalWaterTick::Settled);
        }
        let state = Self.get_flowing(water, Level::L7, false).to_state_id(water);
        Some(RegionalWaterTick::SpreadToSides {
            sides,
            state,
            delay: WATER_FLOW_SPEED,
        })
    }
}

impl FluidBehaviour for FlowingWater {
    fn placed(
        &self,
        world: &Arc<World>,
        fluid: &Fluid,
        state_id: BlockStateId,
        block_pos: &BlockPos,
        old_state_id: BlockStateId,
        _notify: bool,
    ) {
        if old_state_id != state_id {
            world.schedule_fluid_tick(fluid, *block_pos, WATER_FLOW_SPEED, TickPriority::Normal);
        }
    }

    fn on_scheduled_tick(&self, world: &Arc<World>, _fluid: &Fluid, block_pos: &BlockPos) {
        Self.on_scheduled_tick_internal(world, &Fluid::FLOWING_WATER, block_pos);
    }

    fn on_neighbor_update(
        &self,
        world: &Arc<World>,
        fluid: &Fluid,
        block_pos: &BlockPos,
        _notify: bool,
    ) {
        // Avoid rescheduling a fluid tick if one is already queued.
        if !world.is_fluid_tick_scheduled(block_pos, fluid) {
            world.schedule_fluid_tick(fluid, *block_pos, WATER_FLOW_SPEED, TickPriority::Normal);
        }
    }

    fn on_entity_collision(&self, entity: &dyn EntityBase) {
        entity.get_entity().extinguish();
    }
}

impl FlowingFluid for FlowingWater {
    fn get_level_decrease_per_block(&self, _world: &World) -> i32 {
        1
    }

    fn get_flow_speed(&self, _world: &World) -> u8 {
        WATER_FLOW_SPEED
    }

    fn get_max_flow_distance(&self, _world: &World) -> i32 {
        4
    }

    /// Determines if water can convert to source blocks based on game rules.
    fn can_convert_to_source(&self, world: &Arc<World>) -> bool {
        world.level_info.load().game_rules.water_source_conversion
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_data::Block;

    #[tokio::test]
    async fn regional_water_selects_the_nearest_hole_in_owned_terrain() {
        let folder = tempfile::tempdir().unwrap();
        let world = crate::entity::ai::test_support::world(&folder);
        let water = &Fluid::FLOWING_WATER;
        let source = FlowingWater.get_source(water, false).to_state_id(water);
        let home = BlockPos::new(8, 64, 8);
        let floor = |at: BlockPos| {
            Some(if at.0.y == 63 {
                Block::STONE.default_state.id
            } else {
                BlockStateId::AIR
            })
        };
        let selected = |read: &dyn Fn(BlockPos) -> Option<BlockStateId>| {
            let Some(RegionalWaterTick::SpreadToSides {
                sides,
                state,
                delay,
            }) = FlowingWater::regional_water_tick(&world, home, source, read)
            else {
                panic!("owned source should spread into its selected neighboring blocks");
            };
            assert_eq!(
                state,
                FlowingWater
                    .get_flowing(water, Level::L7, false)
                    .to_state_id(water)
            );
            assert_eq!(delay, WATER_FLOW_SPEED);
            sides
        };
        assert_eq!(selected(&floor), BlockDirection::horizontal().map(Some));
        let beside_hole = |at| {
            if at == BlockPos::new(10, 63, 8) {
                Some(BlockStateId::AIR)
            } else {
                floor(at)
            }
        };
        let [north, south, west, east] = BlockDirection::horizontal();
        assert_eq!(selected(&beside_hole), [None, None, None, Some(east)]);
        let walled = |at| {
            if at == home.offset(west.to_offset()) || at == home.offset(east.to_offset()) {
                Some(Block::STONE.default_state.id)
            } else {
                floor(at)
            }
        };
        assert_eq!(selected(&walled), [Some(north), Some(south), None, None]);
        let missing = |at| {
            (at != BlockPos::new(10, 63, 8))
                .then(|| floor(at))
                .flatten()
        };
        assert!(FlowingWater::regional_water_tick(&world, home, source, &missing).is_none());
        let edge = BlockPos::new(15, 64, 8);
        assert!(FlowingWater::regional_water_tick(&world, edge, source, &floor).is_none());
        world.level.shutdown().await;
    }
}
