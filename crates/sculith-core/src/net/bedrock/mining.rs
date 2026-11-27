use std::{
    sync::{Arc, MutexGuard, PoisonError, atomic::Ordering::Relaxed},
    time::Instant,
};

use pumpkin_data::{Block, BlockState, BlockStateId};
use sculith_protocol::{
    RawPacket,
    bedrock::{
        client::CUpdateBlock,
        server::{
            player_action::{PlayerActionType, SPlayerAction},
            player_auth_input::{InputData, SPlayerAuthInput},
        },
    },
    codec::var_ulong::VarULong,
    packet::Packet,
    serial::PacketRead,
};
use sculith_util::{
    GameMode,
    math::{position::BlockPos, vector2::Vector2, vector3::Vector3},
};

use crate::{
    entity::{
        EntityBase,
        player::{BedrockMiningTool, Player},
    },
    server::{Server, TickDeadline},
    world::{BlockBreakingProgress, World},
};

use super::BedrockClient;

pub(super) const MIN_PREDICTED_BREAK_PROGRESS: f32 = 0.65;

#[derive(PartialEq, Eq)]
struct MiningTarget {
    world: uuid::Uuid,
    position: BlockPos,
    state: BlockStateId,
    start_tick: i32,
}

impl MiningTarget {
    fn new(player: &Player, world: &World, position: BlockPos, state: BlockStateId) -> Self {
        Self {
            world: world.uuid,
            position,
            state,
            start_tick: player.start_mining_time.load(Relaxed),
        }
    }
}

pub(super) struct MiningClock {
    target: MiningTarget,
    player_position: Vector3<f64>,
    elapsed: i32,
    last_step: Option<Instant>,
    invalidated: bool,
}

impl MiningClock {
    fn start(
        player: &Player,
        world: &World,
        position: BlockPos,
        state: BlockStateId,
        deadline: Option<TickDeadline>,
    ) -> Self {
        let start_tick = player.tick_counter.load(Relaxed);
        player.start_mining_time.store(start_tick, Relaxed);
        Self {
            target: MiningTarget {
                world: world.uuid,
                position,
                state,
                start_tick,
            },
            player_position: player.get_entity().pos.load(),
            elapsed: 1,
            last_step: deadline.map(|step| step.at),
            invalidated: false,
        }
    }

    fn advance(&mut self, deadline: Option<TickDeadline>) -> bool {
        if let Some((next, previous)) = deadline.zip(self.last_step)
            && !next.follows(previous)
        {
            return false;
        }
        self.elapsed = self.elapsed.saturating_add(1);
        self.last_step = deadline.map(|step| step.at);
        true
    }
}

enum RegionalMiningAction {
    Handled,
    Removed(BlockStateId),
}

impl BedrockClient {
    const fn is_regional_mining_action(action: PlayerActionType) -> bool {
        matches!(
            action,
            PlayerActionType::StartDestroyBlock
                | PlayerActionType::AbortDestroyBlock
                | PlayerActionType::PredictDestroyBlock
                | PlayerActionType::StopDestroyBlock
        )
    }

    pub(super) fn inline_mining_action(
        packet: &SPlayerAuthInput,
    ) -> Option<(PlayerActionType, BlockPos)> {
        let [action] = packet.block_actions.as_deref()? else {
            return None;
        };
        (packet.item_interaction.is_none()
            && packet.item_stack_request.is_none()
            && packet
                .input_data
                .get(InputData::PerformBlockActions as usize)
            && Self::is_regional_mining_action(action.action))
        .then_some((action.action, action.block_pos))
    }

    pub(super) fn can_route_mining_action(packet: &RawPacket) -> bool {
        if packet.id != SPlayerAction::PACKET_ID || packet.payload.len() > 64 {
            return false;
        }
        let payload = &mut packet.payload.as_ref();
        VarULong::read(payload).is_ok()
            && PlayerActionType::read(payload).is_ok_and(Self::is_regional_mining_action)
    }

    pub(super) fn try_regional_mining_action(
        &self,
        player: &Arc<Player>,
        server: &Server,
        deadline: TickDeadline,
        packet: &RawPacket,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<()> {
        let mut payload = packet.payload.as_ref();
        let action = SPlayerAction::read(&mut payload).ok()?;
        if !payload.is_empty()
            || action.player_runtime_id.0 != u64::try_from(player.get_entity().entity_id).ok()?
        {
            return None;
        }
        self.try_apply_regional_mining_action(
            player,
            server,
            deadline,
            (action.action, action.block_position),
            owns,
        )
    }

    pub(super) fn try_apply_regional_mining_action(
        &self,
        player: &Arc<Player>,
        server: &Server,
        deadline: TickDeadline,
        action: (PlayerActionType, BlockPos),
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<()> {
        if !player.has_client_loaded() || player.has_delayed_destroy.load(Relaxed) {
            return None;
        }
        let world = player.world();
        let outcome = {
            // World mining holds the reader while checking tools and committing a break.
            let _tool = player.try_regional_inventory_edit()?;
            let teleport = player.awaiting_teleport.try_lock().ok()?;
            if teleport.is_some() {
                return None;
            }
            player
                .get_entity()
                .bounding_box
                .try_with_current_world(&world, || {
                    self.apply_regional_mining_action(
                        player, &world, server, deadline, action, owns,
                    )
                })??
        };
        if let RegionalMiningAction::Removed(state) = outcome {
            world.queue_regional_edit_update(action.1, state, BlockStateId::AIR);
        }
        Some(())
    }

    fn apply_regional_mining_action(
        &self,
        player: &Arc<Player>,
        world: &World,
        server: &Server,
        deadline: TickDeadline,
        (action, location): (PlayerActionType, BlockPos),
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<RegionalMiningAction> {
        let mut position = player.mining_pos.try_lock().ok()?;
        let mut clock = self.activity.mining.try_lock().ok()?;
        let target = if matches!(action, PlayerActionType::AbortDestroyBlock) {
            *position
        } else {
            location
        };
        let active =
            player.mining.load(Relaxed) || player.current_block_destroy_stage.load(Relaxed) >= 0;
        let eligible = || {
            player.can_simulate_in(world)
                && player.has_stable_world_membership()
                && !player.living_entity.is_dead_or_dying()
                && !server.tick_rate_manager.is_frozen()
                && player.gamemode.load() == GameMode::Survival
                && owns(player.get_entity().chunk_pos.load())
                && owns(target.chunk_position())
        };
        if !eligible() {
            return None;
        }
        let outcome = match action {
            PlayerActionType::StartDestroyBlock if !active => {
                world
                    .level
                    .try_with_block_state(target, |_, state| {
                        let block = Block::from_state_id(state);
                        if state.to_state().is_air()
                            || server.block_registry.get_pumpkin_block(block.id).is_some()
                        {
                            return None;
                        }
                        let (speed, _) = player.try_bedrock_mining_progress(
                            server,
                            &target,
                            state.to_state(),
                            block,
                        )?;
                        let stage = (speed * 10.0) as i32;
                        world.try_publish_regional_block_breaking(
                            player,
                            target,
                            BlockBreakingProgress::Start { stage, speed },
                            None,
                            || {
                                if !eligible()
                                    || !world.offer_regional_mining(self.id, player, target)
                                {
                                    return None;
                                }
                                *position = target;
                                *clock = Some(MiningClock::start(
                                    player,
                                    world,
                                    target,
                                    state,
                                    Some(deadline),
                                ));
                                player.current_block_destroy_stage.store(stage, Relaxed);
                                player
                                    .current_block_breaking_speed
                                    .store(speed.to_bits(), Relaxed);
                                player.mining.store(true, Relaxed);
                                Some(())
                            },
                        )
                    })
                    .flatten()?;
                RegionalMiningAction::Handled
            }
            PlayerActionType::AbortDestroyBlock if active => {
                world.try_publish_regional_block_breaking(
                    player,
                    target,
                    BlockBreakingProgress::Stop,
                    None,
                    || eligible().then(|| Self::clear_regional_mining_progress(player)),
                )?;
                *clock = None;
                RegionalMiningAction::Handled
            }
            PlayerActionType::PredictDestroyBlock | PlayerActionType::StopDestroyBlock
                if player.mining.load(Relaxed) && *position == location =>
            {
                self.try_regional_mining_prediction(
                    world,
                    server,
                    player,
                    &mut clock,
                    (action, location),
                    &|chunk| eligible() && owns(chunk),
                )?
            }
            _ => return None,
        };
        player.update_last_action_time();
        tracing::trace!(player = %player.gameprofile.name, ?target, ?action,
            "Applied Bedrock mining action on its region");
        Some(outcome)
    }

    fn try_regional_mining_prediction(
        &self,
        world: &World,
        server: &Server,
        player: &Player,
        session: &mut Option<MiningClock>,
        (action, position): (PlayerActionType, BlockPos),
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<RegionalMiningAction> {
        let clock = session.as_mut().filter(|clock| {
            clock.target.position == position && clock.target.world == world.uuid
        })?;
        let live = world.level.try_read_block_state(position)?;
        clock.invalidated |= live != clock.target.state;
        let stale = clock.invalidated;
        // The input drain owns the FIFO head. Packets behind this action remain ordered.
        let can_publish =
            || owns(position.chunk_position()) && owns(player.get_entity().chunk_pos.load());
        let outcome = Self::try_with_regional_mining_speed(
            world,
            server,
            player,
            clock,
            can_publish,
            |speed, stage, tool, eligible| {
                if !stale && speed * clock.elapsed as f32 >= MIN_PREDICTED_BREAK_PROGRESS {
                    Self::try_commit_regional_mining_break(
                        world,
                        server,
                        player,
                        &clock.target,
                        tool,
                        owns,
                        eligible,
                    )?;
                    return Some(RegionalMiningAction::Removed(live));
                }
                let progress = if stale || matches!(action, PlayerActionType::StopDestroyBlock) {
                    BlockBreakingProgress::Stop
                } else {
                    BlockBreakingProgress::Update {
                        stage,
                        speed: Some(speed),
                    }
                };
                self.try_correct_regional_mining(
                    world, player, position, live, progress, eligible,
                )?;
                Some(RegionalMiningAction::Handled)
            },
        )?;
        if !player.mining.load(Relaxed) {
            *session = None;
        }
        Some(outcome)
    }

    fn try_correct_regional_mining(
        &self,
        world: &World,
        player: &Player,
        position: BlockPos,
        state: BlockStateId,
        progress: BlockBreakingProgress,
        eligible: &dyn Fn() -> bool,
    ) -> Option<()> {
        world.level.try_with_block_state(position, |_, live| {
            if live != state {
                return None;
            }
            let update = self
                .serialize_packet(&CUpdateBlock::new(
                    position,
                    BlockState::to_be_network_id(state),
                ))
                .ok()?;
            world.try_publish_regional_block_breaking(player, position, progress, None, || {
                if !eligible() || !self.try_enqueue_packet_data_unobserved(update) {
                    return None;
                }
                if matches!(progress, BlockBreakingProgress::Stop) {
                    Self::clear_regional_mining_progress(player);
                }
                Some(())
            })
        })?
    }

    fn clear_regional_mining_progress(player: &Player) {
        player.mining.store(false, Relaxed);
        player.current_block_destroy_stage.store(-1, Relaxed);
        player.current_block_breaking_speed.store(0, Relaxed);
    }

    fn mining_clock(&self) -> MutexGuard<'_, Option<MiningClock>> {
        self.activity
            .mining
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn clear_mining_clock(&self) {
        *self.mining_clock() = None;
    }

    pub(super) fn begin_world_mining(
        &self,
        player: &Player,
        world: &World,
        position: BlockPos,
        state: BlockStateId,
    ) {
        *self.mining_clock() = Some(MiningClock::start(player, world, position, state, None));
    }

    /// Uses the same clock for world steps and region steps. The world clock is
    /// also the baseline for sessions established outside an active extent.
    pub(crate) fn tick_mining_clock(
        &self,
        server: &Server,
        player: &Arc<Player>,
        world: &World,
        position: BlockPos,
        state: BlockStateId,
        deadline: Option<TickDeadline>,
    ) -> Option<i32> {
        let target = MiningTarget::new(player, world, position, state);
        let baseline = player.mining_ticks_since(target.start_tick);
        let native_free = server.plugin_manager.try_without_native_code();
        let mut work = self.mining_clock();
        if work
            .as_ref()
            .is_some_and(|clock| clock.invalidated || clock.target != target)
        {
            return None;
        }
        if native_free.is_none()
            || !self.is_current_protocol()
            || player.gamemode.load() != GameMode::Survival
            || !player.can_interact_with_block_at(&position, 1.0)
        {
            if let Some(clock) = work.as_mut() {
                clock.elapsed = baseline;
                clock.last_step = None;
            }
            return Some(baseline);
        }
        let player_position = player.get_entity().pos.load();
        let requested = deadline.is_some()
            && !server.tick_rate_manager.is_frozen()
            && world.offer_regional_mining(self.id, player, position);
        if let Some(clock) = work.as_mut() {
            clock.player_position = player_position;
            clock.advance(deadline);
            clock.elapsed = clock.elapsed.max(baseline);
            // During a slow world step an abort or tool change can arrive
            // after that connection's packet drain. Keep it ahead of new progress.
            if clock.elapsed > baseline
                && self
                    .activity
                    .inbound_head
                    .try_lock()
                    .is_ok_and(|head| head.is_none() && player.inbound_packets.is_empty())
            {
                return Some(clock.elapsed);
            }
            return Some(baseline);
        }
        *work = requested.then(|| MiningClock {
            target,
            player_position,
            elapsed: baseline,
            last_step: deadline.map(|step| step.at),
            invalidated: false,
        });
        Some(baseline)
    }

    pub(crate) fn mining_prediction_ticks(
        &self,
        server: &Server,
        player: &Player,
        world: &World,
        position: BlockPos,
        state: BlockStateId,
        baseline: i32,
    ) -> Option<i32> {
        let work = self.mining_clock();
        let Some(clock) = work.as_ref() else {
            return Some(baseline);
        };
        let target = MiningTarget::new(player, world, position, state);
        if clock.invalidated || clock.target != target {
            return None;
        }
        if !self.is_current_protocol() || player.gamemode.load() != GameMode::Survival {
            return Some(baseline);
        }
        let Some(_native) = server.plugin_manager.try_without_native_code() else {
            return Some(baseline);
        };
        Some(clock.elapsed.max(baseline))
    }

    /// Advances the shared clock and handles a steady break within the region when possible.
    pub(crate) fn advance_regional_mining(
        &self,
        world: &World,
        server: &Server,
        player: &Player,
        deadline: TickDeadline,
        owns: impl Fn(Vector2<i32>) -> bool,
    ) -> bool {
        let Ok(mut work) = self.activity.mining.try_lock() else {
            return true;
        };
        let Some(clock) = work.as_mut() else {
            return false;
        };
        if clock.invalidated
            || !player.mining.load(Relaxed)
            || player.start_mining_time.load(Relaxed) != clock.target.start_tick
            || player.gamemode.load() != GameMode::Survival
            || player.living_entity.is_dead_or_dying()
        {
            return false;
        }
        let entity = player.get_entity();
        if !owns(entity.chunk_pos.load()) {
            return true;
        }
        let position = clock.target.position;
        if clock.target.world != world.uuid
            || entity.pos.load() != clock.player_position
            || !owns(position.chunk_position())
        {
            return false;
        }

        let mut ready = false;
        let current = world.level.try_with_block_state(position, |_, state| {
            if clock.target.state != state {
                return false;
            }
            if owns(position.chunk_position())
                && owns(entity.chunk_pos.load())
                && entity.pos.load() == clock.player_position
                && clock.advance(Some(deadline))
            {
                tracing::trace!(
                    player = %player.gameprofile.name,
                    elapsed = clock.elapsed,
                    "Advanced regional Bedrock mining"
                );
                let speed = f32::from_bits(player.current_block_breaking_speed.load(Relaxed));
                let progress = speed * clock.elapsed as f32;
                ready = progress >= 1.0;
                if !ready
                    && BlockBreakingProgress::stage_at(progress)
                        > player.current_block_destroy_stage.load(Relaxed)
                {
                    let _ =
                        self.try_publish_regional_mining_stage(world, server, player, clock, &owns);
                }
            }
            true
        });
        if current == Some(false) {
            clock.invalidated = true;
            return false;
        }
        if ready
            && self
                .try_finish_regional_mining(world, server, player, clock, &owns)
                .is_some()
        {
            let state = clock.target.state;
            *work = None;
            drop(work);
            world.queue_regional_edit_update(position, state, BlockStateId::AIR);
            tracing::trace!(player = %player.gameprofile.name, ?position, "Finished Bedrock mining on its region");
            return false;
        }
        true
    }

    fn try_publish_regional_mining_stage(
        &self,
        world: &World,
        server: &Server,
        player: &Player,
        clock: &MiningClock,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<()> {
        self.try_with_current_regional_mining(
            world,
            server,
            player,
            clock,
            owns,
            |speed, previous, _, eligible| {
                let progress = speed * clock.elapsed as f32;
                let stage = BlockBreakingProgress::stage_at(progress);
                if progress >= 1.0 || stage <= previous {
                    return None;
                }
                // Bedrock viewers already animate steady speed from the start event.
                world.try_publish_regional_block_breaking(
                    player,
                    clock.target.position,
                    BlockBreakingProgress::Update { stage, speed: None },
                    None,
                    || eligible().then(|| player.current_block_destroy_stage.store(stage, Relaxed)),
                )
            },
        )
    }

    fn try_finish_regional_mining(
        &self,
        world: &World,
        server: &Server,
        player: &Player,
        clock: &MiningClock,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<()> {
        self.try_with_current_regional_mining(
            world,
            server,
            player,
            clock,
            owns,
            |speed, _, tool, eligible| {
                if speed * (clock.elapsed as f32) < 1.0 {
                    return None;
                }
                Self::try_commit_regional_mining_break(
                    world,
                    server,
                    player,
                    &clock.target,
                    tool,
                    owns,
                    eligible,
                )
            },
        )
    }

    fn try_commit_regional_mining_break(
        world: &World,
        server: &Server,
        player: &Player,
        target: &MiningTarget,
        tool: BedrockMiningTool,
        owns: &impl Fn(Vector2<i32>) -> bool,
        eligible: &dyn Fn() -> bool,
    ) -> Option<()> {
        let position = target.position;
        let state = target.state;
        world.try_publish_regional_block_breaking(
            player,
            position,
            BlockBreakingProgress::Stop,
            Some(state),
            || {
                world.try_remove_regional_mining_block(
                    server,
                    player,
                    position,
                    state,
                    tool,
                    &|chunk| owns(chunk) && eligible(),
                )?;
                Self::clear_regional_mining_progress(player);
                Some(())
            },
        )
    }

    fn try_with_current_regional_mining<R>(
        &self,
        world: &World,
        server: &Server,
        player: &Player,
        clock: &MiningClock,
        owns: &impl Fn(Vector2<i32>) -> bool,
        apply: impl FnOnce(f32, i32, BedrockMiningTool, &dyn Fn() -> bool) -> Option<R>,
    ) -> Option<R> {
        // World mining takes the tool before checking the clock.
        let _tool = player.try_regional_inventory_edit()?;
        let teleport = player.awaiting_teleport.try_lock().ok()?;
        let head = self.activity.inbound_head.try_lock().ok()?;
        if teleport.is_some()
            || head.is_some()
            || !player.inbound_packets.is_empty()
            || !player.has_client_loaded()
            || player.has_delayed_destroy.load(Relaxed)
            || server.tick_rate_manager.is_frozen()
        {
            return None;
        }

        let entity = player.get_entity();
        entity.bounding_box.try_with_current_world(world, || {
            Self::try_with_regional_mining_speed(
                world,
                server,
                player,
                clock,
                || {
                    owns(entity.chunk_pos.load())
                        && owns(clock.target.position.chunk_position())
                        && player.inbound_packets.is_empty()
                },
                apply,
            )
        })?
    }

    fn try_with_regional_mining_speed<R>(
        world: &World,
        server: &Server,
        player: &Player,
        clock: &MiningClock,
        can_publish: impl Fn() -> bool,
        apply: impl FnOnce(f32, i32, BedrockMiningTool, &dyn Fn() -> bool) -> Option<R>,
    ) -> Option<R> {
        let entity = player.get_entity();
        let position = clock.target.position;
        let speed = player.current_block_breaking_speed.load(Relaxed);
        let previous = player.current_block_destroy_stage.load(Relaxed);
        if previous < 0 {
            return None;
        }
        let eligible = || {
            player.has_stable_world_membership()
                && player.mining.load(Relaxed)
                && clock.target.world == world.uuid
                && player.start_mining_time.load(Relaxed) == clock.target.start_tick
                && player.gamemode.load() == GameMode::Survival
                && !player.living_entity.is_dead_or_dying()
                && entity.pos.load() == clock.player_position
                && can_publish()
                && player.current_block_breaking_speed.load(Relaxed) == speed
                && player.current_block_destroy_stage.load(Relaxed) == previous
        };
        let state = clock.target.state.to_state();
        let block = Block::from_state_id(state.id);
        if !eligible() || server.block_registry.get_pumpkin_block(block.id).is_some() {
            return None;
        }
        let (actual, tool) = player.try_bedrock_mining_progress(server, &position, state, block)?;
        (actual.to_bits() == speed)
            .then(|| apply(actual, previous, tool, &eligible))
            .flatten()
    }
}
