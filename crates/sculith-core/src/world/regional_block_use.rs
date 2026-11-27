use dashmap::try_result::TryResult;
use pumpkin_data::{
    BlockDirection, BlockState, BlockStateId, fluid::Fluid, game_event::GameEvent,
    packet::CURRENT_MC_VERSION, sound::SoundCategory,
};
use sculith_protocol::{IdOr, java::client::play::CSoundEffect};
use sculith_util::math::{
    boundingbox::BoundingBox, position::BlockPos, vector2::Vector2, vector3::Vector3,
};

use crate::{
    block::{
        blocks::{BlockToggle, fences::dry_fence_keeps_state},
        registry::can_replace_with_other_block,
    },
    entity::{EntityBase, player::Player},
    net::{ClientPlatform, java::JavaClient},
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
    pub(crate) fn block_toggle_has_listeners(&self, position: BlockPos, event: GameEvent) -> bool {
        let area = super::game_event::notified_area(
            position.to_centered_f64(),
            event.notification_radius(),
        );
        self.entity_index
            .try_has_game_event_listener(&self.entities, &area)
            != Some(false)
    }

    pub(crate) fn try_toggle_regional_block(
        &self,
        server: &Server,
        position: BlockPos,
        expected: BlockStateId,
        toggle: &BlockToggle,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<()> {
        let plugins = &server.plugin_manager;
        let unobserved = || {
            !plugins.has_handlers::<PlayerInteractEvent>()
                && !plugins.has_handlers::<BlockPhysicsEvent>()
                && !BlockToggle::effects_observed(server)
        };
        let at = position.chunk_position();
        if !unobserved()
            || !owns(at)
            || !self.is_in_build_limit(position)
            || Self::get_fluid_from_state_id(expected) != &Fluid::EMPTY
            || self.players.load().iter().any(|viewer| {
                super::is_within_chebyshev_distance(at, viewer.get_entity().chunk_pos.load(), 1)
                    && match viewer.client.as_ref() {
                        ClientPlatform::Java(client) => client.version.load() != CURRENT_MC_VERSION,
                        ClientPlatform::Bedrock(client) => !client.is_current_protocol(),
                    }
            })
        {
            return None;
        }
        let _checkpoint = server.try_entity_save_guard()?;
        let TryResult::Present(chunk) = self.level.loaded_chunks.try_get(&at) else {
            return None;
        };
        let expected_chunk = chunk.value().clone();
        drop(chunk);
        let changed = self.level.try_apply_regional_block_edit(
            &expected_chunk,
            position,
            expected,
            toggle.state,
            owns,
            |read| {
                let after = |pos| {
                    if pos == position {
                        Some(toggle.state)
                    } else {
                        read(pos)
                    }
                };
                unobserved()
                    && !self.block_toggle_has_listeners(position, toggle.event)
                    && BlockDirection::all().into_iter().all(|direction| {
                        let neighbor = position.offset(direction.to_offset());
                        read(neighbor).is_some_and(|state| {
                            self.is_plain_regional_block(state)
                                || dry_fence_keeps_state(state, neighbor, after)
                        })
                    })
            },
        )?;
        changed.then_some(())
    }

    pub(crate) fn publish_regional_toggle(
        &self,
        player: &Player,
        position: BlockPos,
        expected: BlockStateId,
        toggle: &BlockToggle,
    ) {
        self.queue_regional_edit_update(position, expected, toggle.state);
        let center = position.to_centered_f64();
        let java_sound = CSoundEffect::new(
            IdOr::Id(toggle.java_sound as u16),
            SoundCategory::Blocks,
            &center,
            1.0,
            1.0,
            rand::random(),
        );
        if let Ok(data) = JavaClient::serialize_packet_for_version(&java_sound, CURRENT_MC_VERSION)
        {
            for viewer in self
                .players
                .load()
                .iter()
                .filter(|viewer| viewer.gameprofile.id != player.gameprofile.id)
            {
                if super::is_within_chebyshev_distance(
                    position.chunk_position(),
                    viewer.get_entity().chunk_pos.load(),
                    1,
                ) && let ClientPlatform::Java(client) = viewer.client.as_ref()
                    && client.version.load() == CURRENT_MC_VERSION
                {
                    client.try_enqueue_packet_data_unobserved(data.clone());
                }
            }
        }
        self.send_bedrock_level_sound(
            toggle.bedrock_sound,
            &center,
            BlockState::to_be_network_id(toggle.state) as i32,
            false,
        );
    }

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
                    .try_place_regional_full_block(
                        &expected,
                        target,
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
