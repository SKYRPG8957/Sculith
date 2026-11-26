#[allow(clippy::wildcard_imports)]
use super::*;
use crate::net::bedrock::mining::MIN_PREDICTED_BREAK_PROGRESS;
use pumpkin_data::{BlockState, BlockStateId};

impl BedrockClient {
    pub fn handle_player_action(
        &self,
        player: &Arc<Player>,
        server: &Arc<Server>,
        packet: &SPlayerAction,
    ) {
        let mut spin_attack_prediction = None;
        self.apply_player_action(
            player,
            server,
            packet.action,
            packet.block_position,
            &mut spin_attack_prediction,
        );
        self.reconcile_spin_attack_prediction(player, spin_attack_prediction);
    }

    #[expect(clippy::match_same_arms)]
    #[expect(clippy::too_many_lines)]
    pub(in crate::net::bedrock) fn apply_player_action(
        &self,
        player: &Arc<Player>,
        server: &Arc<Server>,
        action: PlayerAction,
        location: BlockPos,
        spin_attack_prediction: &mut Option<bool>,
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
                            self.begin_world_mining(player, &world, location, state.id);
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
                    let baseline =
                        player.mining_ticks_since(player.start_mining_time.load(Ordering::Relaxed));
                    let active = player.mining.load(Ordering::Relaxed)
                        && *player
                            .mining_pos
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            == location;
                    let elapsed = if active {
                        let Some(elapsed) = self.mining_prediction_ticks(
                            server, player, &world, location, state.id, baseline,
                        ) else {
                            self.sync_mining_block(location, state.id);
                            player.stop_mining();
                            return;
                        };
                        elapsed
                    } else {
                        0
                    };
                    if active && speed * elapsed as f32 >= MIN_PREDICTED_BREAK_PROGRESS {
                        player.stop_mining();
                        if (elapsed > baseline
                            && !player.is_current_mining_target(&world, location, state.id))
                            || !player.commit_block_break(&world, &location, state, gamemode)
                        {
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
            PlayerAction::StartGliding | PlayerAction::StopGliding => {
                player.apply_bedrock_glide_input(
                    server,
                    matches!(action, PlayerAction::StartGliding),
                );
            }
            PlayerAction::StartSwimming | PlayerAction::StopSwimming => {
                player.apply_bedrock_swim_input(matches!(action, PlayerAction::StartSwimming));
            }
            PlayerAction::StartSpinAttack | PlayerAction::StopSpinAttack => {
                *spin_attack_prediction = Some(matches!(action, PlayerAction::StartSpinAttack));
            }
            PlayerAction::StartSprinting => {
                player.apply_sprint_input(server, true);
            }
            PlayerAction::StopSprinting => {
                player.apply_sprint_input(server, false);
            }
            PlayerAction::StartCrawling | PlayerAction::StopCrawling => {
                player.apply_bedrock_crawl_input(matches!(action, PlayerAction::StartCrawling));
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
