use std::sync::Arc;
use std::sync::atomic::Ordering;

use pumpkin_util::math::vector2::Vector2;
use pumpkin_util::math::vector3::Vector3;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::entity::EntityBase;
use crate::server::{Server, ticker};
use crate::world::World;

use super::Player;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum WorldChange {
    Idle,
    Preparing,
    Arriving,
}

pub struct PendingWorldChange {
    player: Arc<Player>,
    pub(super) source: Arc<World>,
}

pub(super) struct WorldDeparture {
    source: Arc<World>,
    pub(super) unwatched: Vec<Vector2<i32>>,
}

impl Drop for WorldDeparture {
    fn drop(&mut self) {
        self.source
            .pending_departures
            .fetch_sub(1, Ordering::Release);
    }
}

impl Drop for PendingWorldChange {
    fn drop(&mut self) {
        self.player.world_change.store(WorldChange::Idle);
    }
}

pub struct WorldTransfer {
    pub(super) player: Arc<Player>,
    pub(super) source: Arc<World>,
    pub(super) destination: Arc<World>,
    pub(super) position: Vector3<f64>,
    pub(super) yaw: f32,
    pub(super) pitch: f32,
    pub(super) completion: oneshot::Sender<Option<WorldDeparture>>,
}

impl WorldTransfer {
    /// Publishes one coherent destination membership between simulation ticks.
    pub(crate) fn apply(self, server: &Server) {
        let departure = self.rebind_player(server);
        let _ = self.completion.send(departure);
    }

    fn rebind_player(&self, server: &Server) -> Option<WorldDeparture> {
        let player = &self.player;
        if self.completion.is_closed()
            || server.stop_token.is_cancelled()
            || player.client.closed()
            || player.world_change.load() != WorldChange::Preparing
            || !Arc::ptr_eq(&player.world(), &self.source)
        {
            return None;
        }
        let worlds = server.worlds.load();
        if !worlds.iter().any(|world| Arc::ptr_eq(world, &self.source))
            || !worlds
                .iter()
                .any(|world| Arc::ptr_eq(world, &self.destination))
        {
            return None;
        }

        let admission = self
            .destination
            .player_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*admission
            || self.destination.players.load().iter().any(|other| {
                other.gameprofile.id == player.gameprofile.id && !Arc::ptr_eq(other, player)
            })
        {
            return None;
        }

        // Keep the source available until its unwatched chunks have been handed to storage.
        self.source
            .pending_departures
            .fetch_add(1, Ordering::AcqRel);
        let mut departure = WorldDeparture {
            source: self.source.clone(),
            unwatched: Vec::new(),
        };
        if !self.source.detach_player(player) {
            return None;
        }

        player.world_change.store(WorldChange::Arriving);
        player.reset_mining();
        departure.unwatched = player.release_world_watchers(&self.source);
        player.change_world_chunks(&self.source.level, &self.destination);
        // Bedrock's spawn latch must be reset before the next world can queue terrain.
        player.bedrock_spawned.store(false, Ordering::Relaxed);
        let entity = player.get_entity();
        entity.set_world(self.destination.clone());
        entity.set_pos(self.position);
        entity.set_rotation(self.yaw, self.pitch);
        entity.last_pos.store(self.position);
        self.destination.register_player(player);
        drop(admission);
        self.source.notify_player_departure(player);
        Some(departure)
    }
}

impl Player {
    pub(crate) fn reserve_world_change(
        self: &Arc<Self>,
        source: &Arc<World>,
    ) -> Option<PendingWorldChange> {
        self.world_change
            .compare_exchange(WorldChange::Idle, WorldChange::Preparing)
            .ok()?;
        let change = PendingWorldChange {
            player: self.clone(),
            source: source.clone(),
        };
        if self.client.closed()
            || !Arc::ptr_eq(&self.world(), source)
            || !source
                .players
                .load()
                .iter()
                .any(|player| Arc::ptr_eq(player, self))
        {
            return None;
        }
        Some(change)
    }

    pub(crate) fn can_tick_in(&self, world: &World) -> bool {
        self.world_change.load() != WorldChange::Arriving
            && std::ptr::eq(self.living_entity.entity.world.load().as_ref(), world)
    }

    /// Schedules a change of world for this connection.
    ///
    /// Tick hooks can return immediately and observe completion through `PlayerChangedWorldEvent`.
    pub fn request_world_teleport(
        self: &Arc<Self>,
        destination: Arc<World>,
        position: Vector3<f64>,
        yaw: Option<f32>,
        pitch: Option<f32>,
    ) {
        self.start_world_teleport(destination, position, yaw, pitch);
    }

    /// Moves the player between worlds. Calls from a simulation thread schedule the move.
    pub async fn teleport_world(
        self: &Arc<Self>,
        destination: Arc<World>,
        position: Vector3<f64>,
        yaw: Option<f32>,
        pitch: Option<f32>,
    ) {
        let wait = self.world().server.upgrade().is_some_and(|server| {
            !ticker::is_ticker_thread() && server.simulation_pool.current_thread_index().is_none()
        });
        if let Some(task) = self.start_world_teleport(destination, position, yaw, pitch)
            && wait
        {
            let _ = task.await;
        }
    }

    fn start_world_teleport(
        self: &Arc<Self>,
        destination: Arc<World>,
        position: Vector3<f64>,
        yaw: Option<f32>,
        pitch: Option<f32>,
    ) -> Option<JoinHandle<()>> {
        let source = self.world();
        let server = source.server.upgrade()?;
        let change = self.reserve_world_change(&source)?;
        let player = self.clone();
        self.spawn_task(async move {
            player
                .finish_world_teleport(server, change, destination, position, yaw, pitch)
                .await;
        })
    }
}
