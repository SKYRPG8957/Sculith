use dashmap::try_result::TryResult;
use pumpkin_data::{
    BlockDirection, BlockState, BlockStateId,
    fluid::Fluid,
    game_event::GameEvent,
    packet::CURRENT_MC_VERSION,
    sound::{Sound, SoundCategory},
};
use sculith_protocol::{IdOr, java::client::play::CSoundEffect};
use sculith_util::math::{
    boundingbox::BoundingBox, position::BlockPos, vector2::Vector2, vector3::Vector3,
};
use sculith_world::level::SyncChunk;

use crate::{
    block::{
        blocks::{
            BlockToggle,
            fences::dry_fence_keeps_state,
            redstone::{buttons::StoneButtonChange, lever::LeverToggle},
        },
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

fn regional_block_effects_unobserved(server: &Server) -> bool {
    !server.plugin_manager.has_handlers::<BlockPhysicsEvent>()
        && !BlockToggle::effects_observed(server)
}

fn regional_activation_unobserved(server: &Server) -> bool {
    !server.plugin_manager.has_handlers::<PlayerInteractEvent>()
        && regional_block_effects_unobserved(server)
}

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

    fn regional_activation_chunk(
        &self,
        server: &Server,
        position: BlockPos,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<SyncChunk> {
        let at = position.chunk_position();
        if !regional_activation_unobserved(server)
            || !owns(at)
            || !self.is_in_build_limit(position)
            || !self.regional_effects_clients_supported(at)
        {
            return None;
        }
        let TryResult::Present(chunk) = self.level.loaded_chunks.try_get(&at) else {
            return None;
        };
        Some(chunk.value().clone())
    }

    fn regional_effects_clients_supported(&self, chunk: Vector2<i32>) -> bool {
        !self.players.load().iter().any(|viewer| {
            super::is_within_chebyshev_distance(chunk, viewer.get_entity().chunk_pos.load(), 1)
                && match viewer.client.as_ref() {
                    ClientPlatform::Java(client) => client.version.load() != CURRENT_MC_VERSION,
                    ClientPlatform::Bedrock(client) => !client.is_current_protocol(),
                }
        })
    }

    pub(crate) fn try_toggle_regional_block(
        &self,
        server: &Server,
        position: BlockPos,
        expected: BlockStateId,
        toggle: &BlockToggle,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<()> {
        let pair = toggle.vertical_pair(position, expected);
        if toggle.changes(position, expected).any(|(pos, old, _)| {
            !self.is_in_build_limit(pos) || Self::get_fluid_from_state_id(old) != &Fluid::EMPTY
        }) {
            return None;
        }
        let expected_chunk = self.regional_activation_chunk(server, position, owns)?;
        let _checkpoint = server.try_entity_save_guard()?;
        let validate = |read: &dyn Fn(BlockPos) -> Option<BlockStateId>| {
            let changed = |pos| {
                toggle
                    .changes(position, expected)
                    .find_map(|(edited, _, new)| (edited == pos).then_some(new))
            };
            let after = |pos| changed(pos).or_else(|| read(pos));
            regional_activation_unobserved(server)
                && !self.block_toggle_has_listeners(position, toggle.event)
                && pair.is_none_or(|(lower, _)| {
                    read(lower.down())
                        .is_some_and(|support| support.to_state().is_side_solid(BlockDirection::Up))
                })
                && toggle.changes(position, expected).all(|(pos, _, _)| {
                    BlockDirection::all().into_iter().all(|direction| {
                        let neighbor = pos.offset(direction.to_offset());
                        changed(neighbor).is_some()
                            || read(neighbor).is_some_and(|state| {
                                self.is_plain_regional_block(state)
                                    || (pair.is_none()
                                        && dry_fence_keeps_state(state, neighbor, after))
                            })
                    })
                })
        };
        if let Some((lower, changes)) = pair {
            self.level.try_apply_regional_vertical_block_pair(
                &expected_chunk,
                lower,
                changes,
                owns,
                validate,
            )?
        } else {
            self.level.try_apply_regional_block_edit(
                &expected_chunk,
                position,
                expected,
                toggle.state,
                owns,
                validate,
            )?
        }
        .then_some(())
    }

    pub(crate) fn try_pull_regional_lever(
        &self,
        server: &Server,
        position: BlockPos,
        expected: BlockStateId,
        toggle: &LeverToggle,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<()> {
        let chunk = self.regional_activation_chunk(server, position, owns)?;
        let _checkpoint = server.try_entity_save_guard()?;
        self.level
            .try_apply_regional_block_edit(
                &chunk,
                position,
                expected,
                toggle.state,
                owns,
                |read| {
                    regional_activation_unobserved(server)
                        && !self.block_toggle_has_listeners(position, toggle.event())
                        && toggle.neighbors_are_inert(position, read, |state| {
                            self.is_plain_regional_block(state)
                        })
                },
            )?
            .then_some(())
    }

    pub(crate) fn publish_regional_lever(
        &self,
        position: BlockPos,
        expected: BlockStateId,
        toggle: &LeverToggle,
    ) {
        self.queue_regional_edit_update(position, expected, toggle.state);
        self.play_regional_java_sound(
            position,
            Sound::BlockLeverClick,
            LeverToggle::JAVA_VOLUME,
            toggle.java_pitch(),
            None,
        );
        self.send_bedrock_level_sound(
            toggle.bedrock_sound(),
            &position.to_centered_f64(),
            LeverToggle::BEDROCK_SOUND_DATA,
            false,
        );
    }

    pub(crate) fn try_press_regional_button(
        &self,
        server: &Server,
        position: BlockPos,
        expected: BlockStateId,
        press: &StoneButtonChange,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<()> {
        self.block_registry.regional_tick(expected.to_block_id())?;
        let chunk = self.regional_activation_chunk(server, position, owns)?;
        if expected == press.state {
            // Consume the click at the time it was seen. If deferred, it could
            // reach the world after the button releases and start a second pulse.
            return (self.level.try_read_block_state(position) == Some(expected)
                && owns(position.chunk_position())
                && regional_activation_unobserved(server))
            .then_some(());
        }
        let _checkpoint = server.try_entity_save_guard()?;
        self.level.try_apply_regional_timed_block_property(
            &chunk,
            &press.release_timer(position),
            (expected, press.state),
            owns,
            |read| {
                !server.plugin_manager.has_handlers::<PlayerInteractEvent>()
                    && self.regional_button_change_allowed(server, position, press, read)
            },
        )
    }

    fn regional_button_change_allowed(
        &self,
        server: &Server,
        position: BlockPos,
        change: &StoneButtonChange,
        read: &dyn Fn(BlockPos) -> Option<BlockStateId>,
    ) -> bool {
        regional_block_effects_unobserved(server)
            && self.regional_effects_clients_supported(position.chunk_position())
            && !self.block_toggle_has_listeners(position, change.event())
            && change
                .neighbors_are_inert(position, read, |state| self.is_plain_regional_block(state))
    }

    pub(super) fn regional_button_release(
        &self,
        position: BlockPos,
        state: BlockStateId,
        read: &dyn Fn(BlockPos) -> Option<BlockStateId>,
    ) -> Option<BlockStateId> {
        let release = StoneButtonChange::prepare(state, false)?;
        if release.state == state {
            return Some(state);
        }
        let server = self.server.upgrade()?;
        self.regional_button_change_allowed(&server, position, &release, read)
            .then_some(release.state)
    }

    pub(crate) fn publish_regional_button_press(
        &self,
        position: BlockPos,
        expected: BlockStateId,
        press: &StoneButtonChange,
    ) {
        self.queue_regional_edit_update(position, expected, press.state);
        self.play_regional_button_effects(position, press);
    }

    pub(super) fn play_regional_button_effects(
        &self,
        position: BlockPos,
        change: &StoneButtonChange,
    ) {
        self.play_regional_java_sound(position, change.java_sound(), 1.0, 1.0, None);
        self.send_bedrock_level_sound(
            change.bedrock_sound(),
            &position.to_centered_f64(),
            StoneButtonChange::BEDROCK_SOUND_DATA,
            false,
        );
    }

    pub(crate) fn publish_regional_toggle(
        &self,
        player: &Player,
        position: BlockPos,
        expected: BlockStateId,
        toggle: &BlockToggle,
    ) {
        self.queue_regional_edit_updates((position, expected, toggle.state), toggle.other_half);
        self.play_regional_java_sound(position, toggle.java_sound, 1.0, 1.0, Some(player));
        self.send_bedrock_level_sound(
            toggle.bedrock_sound,
            &position.to_centered_f64(),
            BlockState::to_be_network_id(toggle.state) as i32,
            false,
        );
    }

    fn play_regional_java_sound(
        &self,
        position: BlockPos,
        sound: Sound,
        volume: f32,
        pitch: f32,
        except: Option<&Player>,
    ) {
        let java_sound = CSoundEffect::new(
            IdOr::Id(sound as u16),
            SoundCategory::Blocks,
            &position.to_centered_f64(),
            volume,
            pitch,
            rand::random(),
        );
        if let Ok(data) = JavaClient::serialize_packet_for_version(&java_sound, CURRENT_MC_VERSION)
        {
            for viewer in self.players.load().iter().filter(|viewer| {
                except.is_none_or(|player| viewer.gameprofile.id != player.gameprofile.id)
            }) {
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
