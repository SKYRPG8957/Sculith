use std::{
    sync::{Arc, MutexGuard, PoisonError, atomic::Ordering::Relaxed},
    time::Instant,
};

use pumpkin_data::BlockStateId;
use pumpkin_util::{
    GameMode,
    math::{position::BlockPos, vector2::Vector2, vector3::Vector3},
};

use crate::{
    entity::{EntityBase, player::Player},
    server::{Server, TickDeadline},
    world::World,
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
