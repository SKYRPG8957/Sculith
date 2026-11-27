use crate::block::blocks::redstone::block_receives_redstone_power;
use crate::block::registry::BlockActionResult;
use crate::block::{
    BlockBehaviour, GetStateForNeighborUpdateArgs, NormalUseArgs, OnNeighborUpdateArgs,
    OnPlaceArgs, PathComputationType,
};
use crate::entity::EntityBase;
use pumpkin_data::BlockDirection;
use pumpkin_data::block_properties::Half;
use pumpkin_data::fluid::Fluid;
use pumpkin_data::sound::{Sound, SoundCategory};
use pumpkin_data::tag::Taggable;
use pumpkin_data::{Block, BlockState, BlockStateId, tag};
use pumpkin_macros::pumpkin_block_from_tag;
use sculith_world::tick::TickPriority;
use sculith_world::world::BlockFlags;

use super::BlockToggle;

type TrapDoorProperties = pumpkin_data::block_properties::OakTrapdoorLikeProperties;

pub(crate) fn manual_trapdoor_toggle(state: BlockStateId) -> Option<BlockToggle> {
    let block = state.to_block();
    if !block.has_tag(&tag::Block::MINECRAFT_TRAPDOORS) || block == &Block::IRON_TRAPDOOR {
        return None;
    }
    let mut properties = TrapDoorProperties::from_state_id(state);
    properties.open = !properties.open;
    Some(BlockToggle::new(
        properties.to_state_id(block),
        properties.open,
        get_sound(block, properties.open),
        ("trapdoor.open", "trapdoor.close"),
    ))
}

fn get_sound(block: &Block, open: bool) -> Sound {
    if open {
        if block.has_tag(&tag::Block::MINECRAFT_WOODEN_TRAPDOORS) {
            Sound::BlockWoodenTrapdoorOpen
        } else if block == &Block::IRON_TRAPDOOR {
            Sound::BlockIronTrapdoorOpen
        } else {
            Sound::BlockCopperTrapdoorOpen
        }
    } else if block.has_tag(&tag::Block::MINECRAFT_WOODEN_TRAPDOORS) {
        Sound::BlockWoodenTrapdoorClose
    } else if block == &Block::IRON_TRAPDOOR {
        Sound::BlockIronTrapdoorClose
    } else {
        Sound::BlockCopperTrapdoorClose
    }
}

#[pumpkin_block_from_tag("minecraft:trapdoors")]
pub struct TrapDoorBlock;

impl BlockBehaviour for TrapDoorBlock {
    fn normal_use(&self, args: NormalUseArgs<'_>) -> BlockActionResult {
        if BlockToggle::apply_in_world(
            Some(args.player),
            args.world,
            *args.position,
            manual_trapdoor_toggle,
        )
        .is_none()
        {
            return BlockActionResult::Pass;
        }
        BlockActionResult::Success
    }

    fn on_place(&self, args: OnPlaceArgs<'_>) -> BlockStateId {
        let mut trapdoor_props = TrapDoorProperties::default(args.block);
        trapdoor_props.waterlogged = args.replacing.water_source();

        let powered = block_receives_redstone_power(args.world, args.position);

        let player_facing = args.player.get_entity().get_horizontal_facing();

        // Correct facing logic using Option unwrap
        let facing = args
            .direction
            .to_horizontal_facing()
            .unwrap_or(player_facing);

        trapdoor_props.facing = facing;

        trapdoor_props.half = match args.direction {
            BlockDirection::Up => Half::Top,
            BlockDirection::Down => Half::Bottom,
            _ => {
                if args.placement.cursor_pos.y > 0.5 {
                    Half::Top
                } else {
                    Half::Bottom
                }
            }
        };

        trapdoor_props.powered = powered;
        trapdoor_props.open = powered;

        trapdoor_props.to_state_id(args.block)
    }

    fn get_state_for_neighbor_update(
        &self,
        args: GetStateForNeighborUpdateArgs<'_>,
    ) -> BlockStateId {
        let trapdoor_props = TrapDoorProperties::from_state_id(args.state_id);
        if trapdoor_props.waterlogged {
            args.world.schedule_fluid_tick(
                &Fluid::WATER,
                *args.position,
                Fluid::WATER.flow_speed as u8,
                TickPriority::Normal,
            );
        }
        args.state_id
    }

    fn on_neighbor_update(&self, args: OnNeighborUpdateArgs<'_>) {
        let block_state = args.world.get_block_state(args.position);
        let mut trapdoor_props = TrapDoorProperties::from_state_id(block_state.id);
        let powered = block_receives_redstone_power(args.world, args.position);

        if powered != trapdoor_props.powered {
            if powered != trapdoor_props.open {
                trapdoor_props.open = powered;

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

            trapdoor_props.powered = powered;

            args.world.set_block_state(
                args.position,
                trapdoor_props.to_state_id(args.block),
                BlockFlags::NOTIFY_LISTENERS,
            );
        }
    }

    fn is_pathfindable(&self, state: &BlockState, computation_type: PathComputationType) -> bool {
        let props = TrapDoorProperties::from_state_id(state.id);
        match computation_type {
            PathComputationType::Land | PathComputationType::Air => props.open,
            PathComputationType::Water => props.waterlogged,
        }
    }
}
