use dashmap::try_result::TryResult;
use pumpkin_data::{BlockDirection, BlockState, BlockStateId};
use sculith_util::math::{
    boundingbox::BoundingBox, position::BlockPos, vector2::Vector2, vector3::Vector3,
};

use crate::{
    block::registry::can_replace_with_other_block,
    entity::{EntityBase, player::Player},
    plugin::{
        api::events::{
            block::block_physics::BlockPhysicsEvent,
            player::player_statistic_increment::PlayerStatisticIncrementEvent,
        },
        block::{block_can_build::BlockCanBuildEvent, block_place::BlockPlaceEvent},
        player::player_interact_event::PlayerInteractEvent,
    },
    server::Server,
};

use super::World;

impl World {
    /// Writes a plain full block into air. The player and inventory stay pinned
    /// until the caller finishes the same edit and accounts for the item.
    pub(crate) fn try_place_regional_block(
        &self,
        server: &Server,
        player: &Player,
        clicked: BlockPos,
        target: BlockPos,
        state: BlockStateId,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<()> {
        let plugins = &server.plugin_manager;
        let unobserved = || {
            !plugins.has_handlers::<PlayerInteractEvent>()
                && !plugins.has_handlers::<BlockCanBuildEvent>()
                && !plugins.has_handlers::<BlockPlaceEvent>()
                && !plugins.has_handlers::<BlockPhysicsEvent>()
                && !plugins.has_handlers::<PlayerStatisticIncrementEvent>()
        };
        if !unobserved()
            || !owns(clicked.chunk_position())
            || !owns(target.chunk_position())
            || !self.is_in_build_limit(clicked)
            || !self.is_in_build_limit(target)
            || self.is_in_spawn_protection(player, &target)
            || !self.is_plain_regional_block(BlockStateId::AIR)
            || !self.is_plain_regional_block(state)
            || self.players.load().iter().any(|viewer| {
                super::is_within_chebyshev_distance(
                    target.chunk_position(),
                    viewer.get_entity().chunk_pos.load(),
                    1,
                ) && viewer
                    .client
                    .bedrock()
                    .is_some_and(|client| !client.is_current_protocol())
            })
        {
            return None;
        }
        let area = BoundingBox::from_block(&target);
        let mut shapes = state.to_state().get_block_collision_shapes_at(&target);
        if !shapes.next().is_some_and(|shape| {
            shape.min == Vector3::default() && shape.max == Vector3::new(1.0, 1.0, 1.0)
        }) || shapes.next().is_some()
        {
            return None;
        }
        let TryResult::Present(chunk) = self.level.loaded_chunks.try_get(&target.chunk_position())
        else {
            return None;
        };
        let expected = chunk.value().clone();
        drop(chunk);
        self.entity_index
            .try_with_placement_space(self, player, &area, |same_members| {
                self.level
                    .try_apply_regional_block_edit(
                        &expected,
                        target,
                        BlockStateId::AIR,
                        state,
                        |chunk| owns(chunk) && same_members(),
                        |read| {
                            unobserved()
                                && read(clicked).is_some_and(|clicked| {
                                    !can_replace_with_other_block(
                                        clicked.to_block(),
                                        clicked.to_state(),
                                    )
                                })
                                && BlockDirection::all().into_iter().all(|direction| {
                                    read(target.offset(direction.to_offset())).is_some_and(
                                        |neighbor| self.is_plain_regional_block(neighbor),
                                    )
                                })
                        },
                    )?
                    .then_some(())
            })
    }

    pub(crate) fn publish_regional_placement(&self, position: BlockPos, state: BlockStateId) {
        self.queue_regional_edit_update(position, BlockStateId::AIR, state);
        self.send_bedrock_level_sound(
            "place",
            &position.to_centered_f64(),
            BlockState::to_be_network_id(state) as i32,
            false,
        );
    }
}
