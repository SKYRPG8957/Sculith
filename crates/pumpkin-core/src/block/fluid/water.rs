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
    Block, BlockDirection, BlockStateId,
    fluid::{Fluid, FluidProperties, Level},
};
use pumpkin_util::math::position::BlockPos;
use pumpkin_world::{chunk::DownwardWaterTick, tick::TickPriority};
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

    pub(crate) fn regional_downward_tick(
        world: &World,
        position: BlockPos,
        state: BlockStateId,
        read: &dyn Fn(BlockPos) -> Option<BlockStateId>,
    ) -> Option<DownwardWaterTick> {
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
        if (destination != BlockStateId::AIR && destination != falling)
            || !physics::can_be_replaced(destination.to_state(), destination.to_block(), water)
            || world
                .block_registry
                .get_pumpkin_block(Block::AIR.id)
                .is_some()
            || world
                .block_registry
                .get_pumpkin_block(Block::WATER.id)
                .is_some()
        {
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
            let neighbor = read(at)?;
            let fluid = World::get_fluid_from_state_id(neighbor);
            if fluid != &Fluid::EMPTY
                || world.block_registry.get_pumpkin_fluid(fluid.id).is_some()
                || world
                    .block_registry
                    .get_pumpkin_block(neighbor.to_block_id())
                    .is_some()
            {
                return None;
            }
        }

        Some(if destination == falling {
            DownwardWaterTick::Settled
        } else {
            DownwardWaterTick::Spread {
                state: falling,
                delay: WATER_FLOW_SPEED,
            }
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
