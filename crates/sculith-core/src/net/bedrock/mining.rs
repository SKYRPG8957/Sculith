use std::{
    sync::{Arc, MutexGuard, PoisonError, atomic::Ordering::Relaxed},
    time::Instant,
};

use pumpkin_data::{Block, BlockStateId};
use sculith_protocol::{
    RawPacket,
    bedrock::server::player_action::{PlayerActionType, SPlayerAction},
    codec::var_ulong::VarULong,
    packet::Packet,
    serial::PacketRead,
};
use sculith_util::{
    GameMode,
    math::{position::BlockPos, vector2::Vector2, vector3::Vector3},
};

use crate::{
    entity::{EntityBase, player::Player},
    server::{Server, TickDeadline},
    world::{BlockBreakingProgress, World},
};

use super::BedrockClient;

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
}

impl MiningClock {
    fn start_on_region(
        player: &Player,
        world: &World,
        position: BlockPos,
        state: BlockStateId,
        deadline: TickDeadline,
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
            last_step: Some(deadline.at),
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

impl BedrockClient {
    pub(super) fn can_route_mining_action(packet: &RawPacket) -> bool {
        if packet.id != SPlayerAction::PACKET_ID || packet.payload.len() > 64 {
            return false;
        }
        let payload = &mut packet.payload.as_ref();
        VarULong::read(payload).is_ok()
            && matches!(
                PlayerActionType::read(payload),
                Ok(PlayerActionType::StartDestroyBlock | PlayerActionType::AbortDestroyBlock)
            )
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
        let entity = player.get_entity();
        if !payload.is_empty()
            || action.player_runtime_id.0 != u64::try_from(entity.entity_id).ok()?
            || !player.has_client_loaded()
            || player.has_delayed_destroy.load(Relaxed)
        {
            return None;
        }
        // World mining already holds the reader while checking tools, applying
        // progress and committing a break. The region owns the writer here.
        let _tool = player.try_regional_inventory_edit()?;
        let teleport = player.awaiting_teleport.try_lock().ok()?;
        if teleport.is_some() {
            return None;
        }
        let world = player.world();
        entity
            .bounding_box
            .try_with_current_world(&world, || {
                self.apply_regional_mining_action(player, &world, server, deadline, &action, owns)
            })
            .flatten()
    }

    fn apply_regional_mining_action(
        &self,
        player: &Arc<Player>,
        world: &World,
        server: &Server,
        deadline: TickDeadline,
        action: &SPlayerAction,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<()> {
        let mut position = player.mining_pos.try_lock().ok()?;
        let mut clock = self.activity.mining.try_lock().ok()?;
        let (starting, target) = match action.action {
            PlayerActionType::StartDestroyBlock => (true, action.block_position),
            PlayerActionType::AbortDestroyBlock => (false, *position),
            _ => return None,
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
        if starting == active || !eligible() {
            return None;
        }
        if starting {
            world
                .level
                .try_with_block_state(target, |_, state| {
                    let block = Block::from_state_id(state);
                    if state.to_state().is_air()
                        || server.block_registry.get_pumpkin_block(block.id).is_some()
                    {
                        return None;
                    }
                    let speed = player.try_bedrock_mining_progress(
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
                        || {
                            if !eligible() || !world.offer_regional_mining(self.id, player, target)
                            {
                                return None;
                            }
                            *position = target;
                            *clock = Some(MiningClock::start_on_region(
                                player, world, target, state, deadline,
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
        } else {
            world.try_publish_regional_block_breaking(
                player,
                target,
                BlockBreakingProgress::Stop,
                || {
                    eligible().then(|| {
                        player.mining.store(false, Relaxed);
                        player.current_block_destroy_stage.store(-1, Relaxed);
                        player.current_block_breaking_speed.store(0, Relaxed);
                        *clock = None;
                    })
                },
            )?;
        }
        player.update_last_action_time();
        tracing::trace!(player = %player.gameprofile.name, ?target, action = ?action.action,
            "Applied Bedrock mining action on its region");
        Some(())
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
    ) -> i32 {
        let target = MiningTarget::new(player, world, position, state);
        let baseline = player.mining_ticks_since(target.start_tick);
        let Some(_native) = server.plugin_manager.try_without_native_code() else {
            return baseline;
        };
        if !self.is_current_protocol()
            || player.gamemode.load() != GameMode::Survival
            || !player.can_interact_with_block_at(&position, 1.0)
        {
            self.clear_mining_clock();
            return baseline;
        }
        let player_position = player.get_entity().pos.load();
        let mut work = self.mining_clock();
        let requested = deadline.is_some()
            && !server.tick_rate_manager.is_frozen()
            && world.offer_regional_mining(self.id, player, position);
        if let Some(clock) = work.as_mut().filter(|clock| clock.target == target) {
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
                return clock.elapsed;
            }
            return baseline;
        }
        *work = requested.then(|| MiningClock {
            target,
            player_position,
            elapsed: baseline,
            last_step: deadline.map(|step| step.at),
        });
        baseline
    }

    pub(crate) fn mining_prediction_ticks(
        &self,
        server: &Server,
        player: &Player,
        world: &World,
        position: BlockPos,
        state: BlockStateId,
        baseline: i32,
    ) -> i32 {
        if !self.is_current_protocol() || player.gamemode.load() != GameMode::Survival {
            return baseline;
        }
        let Some(_native) = server.plugin_manager.try_without_native_code() else {
            return baseline;
        };
        let target = MiningTarget::new(player, world, position, state);
        self.mining_clock()
            .as_ref()
            .filter(|clock| clock.target == target)
            .map_or(baseline, |clock| clock.elapsed.max(baseline))
    }

    /// Advances the elapsed time that the world will use in its mining checks.
    pub(crate) fn advance_regional_mining(
        &self,
        world: &World,
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
        if !player.mining.load(Relaxed)
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
            }
            true
        });
        if current == Some(false) {
            *work = None;
            return false;
        }
        true
    }
}
