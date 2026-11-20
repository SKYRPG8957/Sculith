use crate::block::{OnNeighborUpdateArgs, OnPlaceArgs, OnScheduledTickArgs};
use pumpkin_data::{Block, BlockDirection, BlockStateId};
use pumpkin_macros::pumpkin_block;
use pumpkin_util::math::position::BlockPos;
use pumpkin_world::{tick::TickPriority, world::BlockFlags};

use crate::block::BlockBehaviour;
use crate::block::registry::BlockRegistry;

use super::block_receives_redstone_power;

type RedstoneLampProperties = pumpkin_data::block_properties::RedstoneOreLikeProperties;

#[pumpkin_block("minecraft:redstone_lamp")]
pub struct RedstoneLamp;

impl RedstoneLamp {
    fn scheduled_replacement(
        state: BlockStateId,
        receives_power: impl FnOnce() -> bool,
    ) -> Option<BlockStateId> {
        if state.to_block_id() != Block::REDSTONE_LAMP.id {
            return None;
        }
        let mut props = RedstoneLampProperties::from_state_id(state);
        if !props.lit || receives_power() {
            return None;
        }
        props.lit = false;
        Some(props.to_state_id(&Block::REDSTONE_LAMP))
    }

    pub(crate) fn regional_replacement(
        registry: &BlockRegistry,
        position: BlockPos,
        state: BlockStateId,
        read: &dyn Fn(BlockPos) -> Option<BlockStateId>,
    ) -> Option<BlockStateId> {
        let Some(replacement) = Self::scheduled_replacement(state, || false) else {
            return Some(state);
        };
        for direction in BlockDirection::all() {
            let neighbor_position = position.offset(direction.to_offset());
            let neighbor = read(neighbor_position)?;
            // Registered neighbors can respond to a shape update or emit power.
            if registry.get_pumpkin_block(neighbor.to_block_id()).is_some() {
                return None;
            }
            if neighbor.to_state().is_solid_block() {
                // A solid neighbor can conduct a strong signal from one block away.
                for side in BlockDirection::all() {
                    let source = neighbor_position.offset(side.to_offset());
                    if source != position
                        && registry
                            .get_pumpkin_block(read(source)?.to_block_id())
                            .is_some()
                    {
                        return None;
                    }
                }
            }
        }
        Some(replacement)
    }
}

impl BlockBehaviour for RedstoneLamp {
    fn on_place(&self, args: OnPlaceArgs<'_>) -> BlockStateId {
        let mut props = RedstoneLampProperties::default(args.block);
        props.lit = block_receives_redstone_power(args.world, args.position);
        props.to_state_id(args.block)
    }

    fn on_neighbor_update(&self, args: OnNeighborUpdateArgs<'_>) {
        {
            let state = args.world.get_block_state(args.position);
            let mut props = RedstoneLampProperties::from_state_id(state.id);
            let is_lit = props.lit;
            let is_receiving_power = block_receives_redstone_power(args.world, args.position);

            if is_lit != is_receiving_power {
                if is_lit {
                    args.world.schedule_block_tick(
                        args.block,
                        *args.position,
                        4,
                        TickPriority::Normal,
                    );
                } else {
                    props.lit = !props.lit;
                    args.world.set_block_state(
                        args.position,
                        props.to_state_id(args.block),
                        BlockFlags::NOTIFY_LISTENERS,
                    );
                }
            }
        }
    }

    fn on_scheduled_tick(&self, args: OnScheduledTickArgs<'_>) {
        let state = args.world.get_block_state_id(args.position);
        if let Some(replacement) = Self::scheduled_replacement(state, || {
            block_receives_redstone_power(args.world, args.position)
        }) {
            args.world.set_block_state_if(
                args.position,
                replacement,
                BlockFlags::NOTIFY_LISTENERS,
                |current| current == state,
            );
        }
    }
}
