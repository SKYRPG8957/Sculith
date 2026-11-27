use std::{cell::Cell, sync::Arc};

use crate::block::{
    CanPlaceAtArgs, EmitsRedstonePowerArgs, GetRedstonePowerArgs, GetStateForNeighborUpdateArgs,
    OnPlaceArgs, OnStateReplacedArgs, blocks::abstract_wall_mounting::WallMountedBlock,
};
use pumpkin_data::{
    Block, BlockDirection, BlockStateId, HorizontalFacingExt,
    block_properties::{AttachFace, LeverLikeProperties},
    game_event::GameEvent,
    sound::{Sound, SoundCategory},
};
use pumpkin_macros::pumpkin_block;
use sculith_util::math::position::BlockPos;
use sculith_world::world::BlockFlags;

use crate::{
    block::{
        registry::BlockActionResult,
        {BlockBehaviour, NormalUseArgs},
    },
    world::World,
};

pub(crate) struct LeverToggle {
    pub state: BlockStateId,
    properties: LeverLikeProperties,
}

impl LeverToggle {
    pub const JAVA_VOLUME: f32 = 0.3;
    pub const BEDROCK_SOUND_DATA: i32 = -1;

    pub(crate) fn from_state(state: BlockStateId) -> Option<Self> {
        if state.to_block() != &Block::LEVER {
            return None;
        }
        let mut properties = LeverLikeProperties::from_state_id(state);
        properties.powered = !properties.powered;
        Some(Self {
            state: properties.to_state_id(&Block::LEVER),
            properties,
        })
    }

    pub(crate) const fn java_pitch(&self) -> f32 {
        if self.properties.powered { 0.6 } else { 0.5 }
    }

    pub(crate) const fn bedrock_sound(&self) -> &'static str {
        if self.properties.powered {
            "power.on"
        } else {
            "power.off"
        }
    }

    pub(crate) const fn event(&self) -> GameEvent {
        if self.properties.powered {
            GameEvent::BlockActivate
        } else {
            GameEvent::BlockDeactivate
        }
    }

    pub(crate) fn neighbors_are_inert(
        &self,
        position: BlockPos,
        read: &dyn Fn(BlockPos) -> Option<BlockStateId>,
        inert: impl Fn(BlockStateId) -> bool,
    ) -> bool {
        let face = self.properties.get_direction();
        let support = position.offset(face.opposite().to_offset());
        if !read(support).is_some_and(|state| inert(state) && state.to_state().is_side_solid(face))
        {
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
}

#[pumpkin_block("minecraft:lever")]
pub struct LeverBlock;

impl BlockBehaviour for LeverBlock {
    fn normal_use(&self, args: NormalUseArgs<'_>) -> BlockActionResult {
        Self::pull(args.world, args.position);
        BlockActionResult::Success
    }

    fn emits_redstone_power(&self, _args: EmitsRedstonePowerArgs<'_>) -> bool {
        true
    }

    fn get_weak_redstone_power(&self, args: GetRedstonePowerArgs<'_>) -> u8 {
        let props = LeverLikeProperties::from_state_id(args.state.id);
        if props.powered { 15 } else { 0 }
    }

    fn get_strong_redstone_power(&self, args: GetRedstonePowerArgs<'_>) -> u8 {
        let props = LeverLikeProperties::from_state_id(args.state.id);
        if props.powered && props.get_direction() == args.direction {
            15
        } else {
            0
        }
    }

    fn on_state_replaced(&self, args: OnStateReplacedArgs<'_>) {
        if !args.moved {
            let lever_props = LeverLikeProperties::from_state_id(args.old_state_id);

            if lever_props.powered {
                Self::update_neighbors(args.world, args.position, lever_props);
            }
        }
    }

    fn on_place(&self, args: OnPlaceArgs<'_>) -> BlockStateId {
        let mut props = LeverLikeProperties::default(args.block);
        (props.face, props.facing) = WallMountedBlock::get_placement_face_for_args(self, &args);

        props.to_state_id(args.block)
    }

    fn can_place_at(&self, args: CanPlaceAtArgs<'_>) -> bool {
        WallMountedBlock::can_place_at_args(self, &args)
    }

    fn get_state_for_neighbor_update(
        &self,
        args: GetStateForNeighborUpdateArgs<'_>,
    ) -> BlockStateId {
        WallMountedBlock::get_state_for_neighbor_update(self, args)
    }
}

impl WallMountedBlock for LeverBlock {
    fn get_direction(&self, state_id: BlockStateId, _block: &Block) -> BlockDirection {
        let props = LeverLikeProperties::from_state_id(state_id);
        match props.face {
            AttachFace::Floor => BlockDirection::Up,
            AttachFace::Ceiling => BlockDirection::Down,
            AttachFace::Wall => props.facing.to_block_direction(),
        }
    }
}

impl LeverBlock {
    pub(crate) fn pull(world: &Arc<World>, block_pos: &BlockPos) {
        if !world.is_in_build_limit(*block_pos) {
            return;
        }
        let mut expected = world.get_block_state_id(block_pos);
        while let Some(toggle) = LeverToggle::from_state(expected) {
            let compared = Cell::new(None);
            if let Some(old) = world.write_block_state_if(block_pos, toggle.state, |_, live| {
                compared.set(Some(live));
                live == expected
            }) {
                world.on_block_state_set(block_pos, old, toggle.state, BlockFlags::NOTIFY_ALL);
                Self::update_neighbors(world, block_pos, toggle.properties);
                let center = block_pos.to_centered_f64();
                world.play_sound_fine(
                    Sound::BlockLeverClick,
                    SoundCategory::Blocks,
                    &center,
                    LeverToggle::JAVA_VOLUME,
                    toggle.java_pitch(),
                );
                world.play_bedrock_level_sound(
                    toggle.bedrock_sound(),
                    &center,
                    LeverToggle::BEDROCK_SOUND_DATA,
                );
                world.emit_game_event(toggle.event().name(), center);
                return;
            }
            let Some(live) = compared.get() else {
                return;
            };
            expected = live;
        }
    }

    fn update_neighbors(
        world: &Arc<World>,
        block_pos: &BlockPos,
        lever_props: LeverLikeProperties,
    ) {
        let direction = lever_props.get_direction().opposite();
        world.update_neighbors(block_pos, None);
        world.update_neighbors(&block_pos.offset(direction.to_offset()), None);
    }
}

pub trait LeverLikePropertiesExt {
    fn get_direction(&self) -> BlockDirection;
}

impl LeverLikePropertiesExt for LeverLikeProperties {
    fn get_direction(&self) -> BlockDirection {
        match self.face {
            AttachFace::Ceiling => BlockDirection::Down,
            AttachFace::Floor => BlockDirection::Up,
            AttachFace::Wall => self.facing.to_block_direction(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_data::block_properties::HorizontalFacing;

    #[test]
    fn regional_lever_checks_neighbors_on_the_far_side_of_its_support() {
        let position = BlockPos::new(4, 80, 4);
        let support = position.offset(BlockDirection::West.to_offset());
        let far_side = support.offset(BlockDirection::West.to_offset());
        let properties = LeverLikeProperties {
            face: AttachFace::Wall,
            facing: HorizontalFacing::East,
            powered: false,
        };
        let Some(toggle) = LeverToggle::from_state(properties.to_state_id(&Block::LEVER)) else {
            panic!("valid lever did not produce a toggle");
        };
        let stone = Block::STONE.default_state.id;
        let lamp = Block::REDSTONE_LAMP.default_state.id;
        let inert = |state| state == stone || state == BlockStateId::AIR;
        let snapshot = |pos| {
            Some(if pos == support {
                stone
            } else {
                BlockStateId::AIR
            })
        };
        assert!(toggle.neighbors_are_inert(position, &snapshot, inert));

        let receiving = |pos| {
            if pos == far_side {
                Some(lamp)
            } else {
                snapshot(pos)
            }
        };
        assert!(!toggle.neighbors_are_inert(position, &receiving, inert));
        let unloaded = |pos| if pos == far_side { None } else { snapshot(pos) };
        assert!(!toggle.neighbors_are_inert(position, &unloaded, inert));
    }
}
