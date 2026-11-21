#[allow(clippy::wildcard_imports)]
use super::*;
use pumpkin_data::{BlockState, BlockStateId};

impl BedrockClient {
    pub fn handle_player_action(
        &self,
        player: &Arc<Player>,
        server: &Arc<Server>,
        packet: &SPlayerAction,
    ) {
        self.apply_player_action(player, server, packet.action, packet.block_position);
    }

    #[expect(clippy::match_same_arms)]
    #[expect(clippy::too_many_lines)]
    pub(super) fn apply_player_action(
        &self,
        player: &Arc<Player>,
        server: &Arc<Server>,
        action: PlayerAction,
        location: BlockPos,
    ) {
        if !player.has_client_loaded()
            || ((player.living_entity.dead.load(Ordering::Relaxed)
                || player.living_entity.health.load() <= 0.0)
                && !matches!(action, PlayerAction::Respawn))
        {
            return;
        }
        player.update_last_action_time();

        match action {
            PlayerAction::StartDestroyBlock
            | PlayerAction::CreativeDestroyBlock
            | PlayerAction::ContinueDestroyBlock => {
                if !player.can_interact_with_block_at(&location, 1.0) {
                    return;
                }

                let entity = &player.get_entity();
                let world = entity.world.load_full();
                let (block, state) = world.get_block_and_state(&location);

                if player.mining.load(Ordering::Relaxed)
                    && *player
                        .mining_pos
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        != location
                {
                    player.stop_mining();
                }

                let gamemode = player.gamemode.load();
                if player.cannot_mine_block(Some(server), block, gamemode) {
                    player.stop_mining();
                    self.sync_mining_block(location, state.id);
                    return;
                }

                if gamemode == GameMode::Creative {
                    player.stop_mining();
                    if !player.commit_block_break(&world, &location, state, gamemode) {
                        self.sync_mining_block(location, world.get_block_state_id(&location));
                    }
                } else if !state.is_air() {
                    let starts_breaking = !player.mining.load(Ordering::Relaxed)
                        || *player
                            .mining_pos
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            != location;
                    if starts_breaking {
                        server
                            .block_registry
                            .attack(&world, block, state, &location, player);
                    }
                    let speed = crate::block::calc_block_breaking(player, state, block);
                    if speed >= 1.0 {
                        player.stop_mining();
                        let broken_state = world.get_block_state(&location);
                        if !player.commit_block_break(&world, &location, broken_state, gamemode) {
                            self.sync_mining_block(location, world.get_block_state_id(&location));
                        }
                    } else {
                        let mut mining_pos = player
                            .mining_pos
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let progress = if starts_breaking {
                            player.start_mining_time.store(
                                player.tick_counter.load(Ordering::Relaxed),
                                Ordering::Relaxed,
                            );
                            player.mining.store(true, Ordering::Relaxed);
                            *mining_pos = location;
                            (speed * 10.0) as i32
                        } else {
                            player.current_block_destroy_stage.load(Ordering::Relaxed)
                        };
                        drop(mining_pos);
                        let old_speed = player
                            .current_block_breaking_speed
                            .swap(speed.to_bits(), Ordering::Relaxed);
                        if starts_breaking {
                            world.set_block_breaking(
                                entity,
                                location,
                                BlockBreakingProgress::Start {
                                    stage: progress,
                                    speed,
                                },
                            );
                            player
                                .current_block_destroy_stage
                                .store(progress, Ordering::Relaxed);
                        } else if old_speed != speed.to_bits() {
                            world.set_block_breaking(
                                entity,
                                location,
                                BlockBreakingProgress::Update {
                                    stage: progress,
                                    speed: Some(speed),
                                },
                            );
                        }
                    }
                }
            }
            action @ (PlayerAction::PredictDestroyBlock | PlayerAction::StopDestroyBlock) => {
                if !player.can_interact_with_block_at(&location, 1.0) {
                    return;
                }

                let entity = &player.get_entity();
                let world = entity.world.load_full();

                let (block, state) = world.get_block_and_state(&location);
                let gamemode = player.gamemode.load();
                if gamemode != GameMode::Creative && !state.is_air() {
                    if player.cannot_mine_block(Some(server), block, gamemode) {
                        player.stop_mining();
                        self.sync_mining_block(location, state.id);
                        return;
                    }
                    let speed = crate::block::calc_block_breaking(player, state, block);
                    let elapsed = player.tick_counter.load(Ordering::Relaxed)
                        - player.start_mining_time.load(Ordering::Relaxed)
                        + 1;
                    let same_block = *player
                        .mining_pos
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        == location;
                    if player.mining.load(Ordering::Relaxed)
                        && same_block
                        && speed * elapsed as f32 >= MIN_PREDICTED_BREAK_PROGRESS
                    {
                        player.stop_mining();
                        if !player.commit_block_break(&world, &location, state, gamemode) {
                            self.sync_mining_block(location, world.get_block_state_id(&location));
                        }
                    } else {
                        self.sync_mining_block(location, state.id);
                        if matches!(action, PlayerAction::StopDestroyBlock) {
                            player.stop_mining();
                        } else {
                            world.set_block_breaking(
                                entity,
                                location,
                                BlockBreakingProgress::Update {
                                    stage: player
                                        .current_block_destroy_stage
                                        .load(Ordering::Relaxed),
                                    speed: Some(speed),
                                },
                            );
                        }
                    }
                } else if matches!(action, PlayerAction::StopDestroyBlock) {
                    player.stop_mining();
                }
            }
            PlayerAction::CrackBlock => {
                // Don't do anything for this action. It is no longer used. Block
                // cracking is done fully server-side.
            }
            PlayerAction::AbortDestroyBlock => {
                player.stop_mining();
            }
            PlayerAction::DropItem => {
                player.drop_held_item(false);
            }
            PlayerAction::StartGliding => {
                Self::start_gliding(player, server);
            }
            PlayerAction::StopGliding => {
                Self::stop_gliding(
                    player,
                    player.get_entity().on_ground.load(Ordering::Relaxed),
                );
            }
            PlayerAction::StartSwimming => {
                player.get_entity().set_swimming(true);
                player.update_player_pose();
            }
            PlayerAction::StopSwimming => {
                player.get_entity().set_swimming(false);
                player.update_player_pose();
            }
            PlayerAction::StartSpinAttack => {
                player
                    .get_entity()
                    .set_pose(pumpkin_data::entity::EntityPose::SpinAttack);
            }
            PlayerAction::StopSpinAttack => {
                player.update_player_pose();
            }
            PlayerAction::StartSprinting => {
                player.apply_sprint_input(server, true);
            }
            PlayerAction::StopSprinting => {
                player.apply_sprint_input(server, false);
            }
            PlayerAction::StartCrawling => {
                player
                    .get_entity()
                    .set_pose(pumpkin_data::entity::EntityPose::Swimming);
            }
            PlayerAction::StopCrawling => {
                player.update_player_pose();
            }
            PlayerAction::StopSleeping => {
                player.wake_up();
            }
            PlayerAction::Respawn
                if player.living_entity.dead.load(Ordering::Relaxed)
                    || player.living_entity.health.load() <= 0.0 =>
            {
                player.request_client_respawn();
            }
            // TODO
            _ => {}
        }
    }

    fn sync_mining_block(&self, position: BlockPos, state: BlockStateId) {
        self.try_enqueue_client_packet(&CUpdateBlock::new(
            position,
            BlockState::to_be_network_id(state),
        ));
    }
}
