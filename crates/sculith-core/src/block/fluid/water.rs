use super::{flowing_trait::FlowingFluid, physics};
use crate::{
    block::{FluidMetadata, fluid::FluidBehaviour},
    entity::EntityBase,
    plugin::api::events::block::{
        block_from_to::BlockFromToEvent, block_physics::BlockPhysicsEvent,
    },
    world::World,
};
use pumpkin_data::{
    BlockDirection, BlockStateId,
    fluid::{Fluid, FluidProperties, Level},
};
use pumpkin_world::{chunk::RegionalWaterTick, tick::TickPriority};
use sculith_util::math::{position::BlockPos, vector3::Vector3};
use std::sync::Arc;

pub struct FlowingWater;

impl FluidMetadata for FlowingWater {
    fn ids() -> Box<[u16]> {
        [Fluid::FLOWING_WATER.id].into()
    }
}

const WATER_FLOW_SPEED: u8 = 5;

impl FlowingWater {
    pub(crate) fn regional_events_unobserved(world: &World) -> bool {
        !world.server.upgrade().is_some_and(|server| {
            server.plugin_manager.has_handlers::<BlockFromToEvent>()
                || server.plugin_manager.has_handlers::<BlockPhysicsEvent>()
        })
    }

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
        if destination != BlockStateId::AIR && destination != falling {
            return None;
        }

        // Dry sides keep the ordinary tick on its vertical path, with no
        // horizontal spreading or source formation.
        for side in BlockDirection::horizontal() {
            let neighbor = read(position.offset(side.to_offset()))?;
            if World::get_fluid_from_state_id(neighbor) != &Fluid::EMPTY {
                return None;
            }
        }
        for side in BlockDirection::all() {
            // A settled destination only needs the horizontal source check;
            // it does not run placement or neighbor callbacks again.
            if destination == falling && matches!(side, BlockDirection::Up | BlockDirection::Down) {
                continue;
            }
            let at = below.offset(side.to_offset());
            if at == position {
                continue;
            }
            if !Self::regional_water_neighbor_is_inert(world, read(at)?) {
                return None;
            }
        }

        Some(if destination == falling {
            RegionalWaterTick::Settled
        } else {
            RegionalWaterTick::SpreadDown {
                state: falling,
                delay: WATER_FLOW_SPEED,
            }
        })
    }

    fn regional_horizontal_tick(
        world: &World,
        position: BlockPos,
        read: &dyn Fn(BlockPos) -> Option<BlockStateId>,
    ) -> Option<RegionalWaterTick> {
        let water = &Fluid::FLOWING_WATER;
        for side in BlockDirection::horizontal() {
            let target = position.offset(side.to_offset());
            if target.chunk_position() != position.chunk_position()
                || read(target)? != BlockStateId::AIR
            {
                return None;
            }
            for direction in BlockDirection::all() {
                let neighbor = target.offset(direction.to_offset());
                if neighbor != position
                    && !Self::regional_water_neighbor_is_inert(world, read(neighbor)?)
                {
                    return None;
                }
            }
        }

        // The ordinary search can look this far from the first side it examines.
        // A solid floor throughout means all four open directions tie.
        let reach = Self.get_max_flow_distance(world) + 1;
        for x in -reach..=reach {
            let width = reach - x.abs();
            for z in -width..=width {
                let floor = read(position.offset(Vector3::new(x, -1, z)))?;
                if physics::can_be_replaced(floor.to_state(), floor.to_block(), water) {
                    return None;
                }
            }
        }
        Some(RegionalWaterTick::SpreadHorizontally {
            state: Self.get_flowing(water, Level::L7, false).to_state_id(water),
            delay: WATER_FLOW_SPEED,
        })
    }

    fn regional_water_neighbor_is_inert(world: &World, state: BlockStateId) -> bool {
        let fluid = World::get_fluid_from_state_id(state);
        fluid == &Fluid::EMPTY
            && world.block_registry.get_pumpkin_fluid(fluid.id).is_none()
            && world
                .block_registry
                .get_pumpkin_block(state.to_block_id())
                .is_none()
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
    async fn regional_water_uses_the_four_way_step_only_within_a_safe_floor() {
        let folder = tempfile::tempdir().unwrap();
        let world = crate::entity::ai::test_support::world(&folder);
        let fluid = &Fluid::FLOWING_WATER;
        let source = FlowingWater.get_source(fluid, false).to_state_id(fluid);
        let home = BlockPos::new(8, 64, 8);
        let floor = |at: BlockPos| {
            Some(if at.0.y == 63 {
                Block::STONE.default_state.id
            } else {
                BlockStateId::AIR
            })
        };
        let Some(RegionalWaterTick::SpreadHorizontally { state, delay }) =
            FlowingWater::regional_water_tick(&world, home, source, &floor)
        else {
            panic!("open stone floor should spread into its four neighboring blocks");
        };
        assert_eq!(
            state,
            FlowingWater
                .get_flowing(fluid, Level::L7, false)
                .to_state_id(fluid)
        );
        assert_eq!(delay, WATER_FLOW_SPEED);
        let beside_hole = |at| {
            if at == BlockPos::new(12, 63, 8) {
                Some(BlockStateId::AIR)
            } else {
                floor(at)
            }
        };
        assert!(FlowingWater::regional_water_tick(&world, home, source, &beside_hole).is_none());
        let edge = BlockPos::new(15, 64, 8);
        assert!(FlowingWater::regional_water_tick(&world, edge, source, &floor).is_none());
        world.level.shutdown().await;
    }
}
