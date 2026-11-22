use std::sync::Arc;
use std::sync::atomic::Ordering;

use pumpkin_util::math::vector2::Vector2;
use pumpkin_util::math::vector3::Vector3;
use tokio::sync::{OwnedMutexGuard, oneshot};
use tokio::task::JoinHandle;

use crate::entity::EntityBase;
use crate::server::{Server, ticker};
use crate::world::World;

use super::Player;
use super::respawn::RespawnCommit;

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

struct WorldDeparture {
    source: Arc<World>,
    unwatched: Vec<Vector2<i32>>,
}

impl Drop for WorldDeparture {
    fn drop(&mut self) {
        self.source
            .pending_operations
            .fetch_sub(1, Ordering::Release);
    }
}

impl Drop for PendingWorldChange {
    fn drop(&mut self) {
        self.player.world_change.store(WorldChange::Idle);
    }
}

#[derive(Clone)]
pub(super) struct WorldPosition {
    pub(super) world: Arc<World>,
    pub(super) position: Vector3<f64>,
    pub(super) yaw: f32,
    pub(super) pitch: f32,
}

enum AppliedWorldChange {
    InPlace,
    Moved(WorldDeparture),
}

pub struct WorldTransfer {
    player: Arc<Player>,
    source: Arc<World>,
    destination: WorldPosition,
    respawn: Option<RespawnCommit>,
    completion: oneshot::Sender<Option<AppliedWorldChange>>,
}

pub struct WorldTransferReservation {
    _source: OwnedMutexGuard<()>,
    _destination: Option<OwnedMutexGuard<()>>,
}

impl WorldTransferReservation {
    pub(crate) const fn for_one_world(owner: OwnedMutexGuard<()>) -> Self {
        Self {
            _source: owner,
            _destination: None,
        }
    }
}

impl WorldTransfer {
    pub(crate) fn try_reserve_worlds(&self) -> Option<WorldTransferReservation> {
        let source = self.source.try_simulation()?;
        let destination = if Arc::ptr_eq(&self.source, &self.destination.world) {
            None
        } else {
            Some(self.destination.world.try_simulation()?)
        };
        Some(WorldTransferReservation {
            _source: source,
            _destination: destination,
        })
    }

    /// Publishes one coherent destination membership between simulation ticks.
    pub(crate) fn apply(self, server: &Server, _reservation: WorldTransferReservation) {
        let departure = self.rebind_player(server);
        let _ = self.completion.send(departure);
    }

    fn rebind_player(&self, server: &Server) -> Option<AppliedWorldChange> {
        let player = &self.player;
        if self.completion.is_closed()
            || server.stop_token.is_cancelled()
            || player.client.closed()
            || player.world_change.load() != WorldChange::Preparing
            || !Arc::ptr_eq(&player.world(), &self.source)
        {
            return None;
        }
        let destination = &self.destination.world;
        let worlds = server.worlds.load();
        if !worlds.iter().any(|world| Arc::ptr_eq(world, &self.source))
            || !worlds.iter().any(|world| Arc::ptr_eq(world, destination))
        {
            return None;
        }

        let login_admission = server.player_admission_state();
        let admission = destination
            .player_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *login_admission
            || !*admission
            || destination.players.load().iter().any(|other| {
                other.gameprofile.id == player.gameprofile.id && !Arc::ptr_eq(other, player)
            })
            || !self
                .source
                .players
                .load()
                .iter()
                .any(|active| Arc::ptr_eq(active, player))
        {
            return None;
        }

        let anchor = match &self.respawn {
            Some(respawn) => respawn.check(player, destination).ok()?,
            None => None,
        };
        let keep_membership = self.respawn.is_some() && Arc::ptr_eq(&self.source, destination);
        let mut departure = if keep_membership {
            None
        } else {
            // Keep the source available until its unwatched chunks have been handed to storage.
            self.source
                .pending_operations
                .fetch_add(1, Ordering::AcqRel);
            let departure = WorldDeparture {
                source: self.source.clone(),
                unwatched: Vec::new(),
            };
            if !self.source.detach_player(player) {
                return None;
            }
            Some(departure)
        };
        if let Some(anchor) = &anchor
            && !anchor.consume(destination)
        {
            if departure.is_some() {
                self.source.register_player(player);
            }
            return None;
        }

        player.world_change.store(WorldChange::Arriving);
        player.reset_mining();
        let entity = player.get_entity();
        if let Some(departure) = &mut departure {
            departure.unwatched = player.release_world_watchers(&self.source);
            player.change_world_chunks(&self.source.level, destination);
            player.bedrock_spawned.store(false, Ordering::Relaxed);
            entity.set_world(destination.clone());
        } else {
            player.set_client_loaded(false);
        }
        entity.set_pos(self.destination.position);
        entity.set_rotation(self.destination.yaw, self.destination.pitch);
        entity.last_pos.store(self.destination.position);
        if departure.is_some() {
            destination.register_player(player);
        }
        if let Some(respawn) = &self.respawn {
            respawn.clear_invalid_spawn(player);
        }
        drop(admission);
        drop(login_admission);
        if let Some(anchor) = anchor {
            anchor.notify(destination);
        }
        Some(departure.map_or_else(
            || AppliedWorldChange::InPlace,
            |departure| {
                self.source.notify_player_departure(player);
                AppliedWorldChange::Moved(departure)
            },
        ))
    }
}

pub(super) fn can_wait_for_world_change(server: &Server) -> bool {
    !ticker::is_ticker_thread() && server.simulation_pool.current_thread_index().is_none()
}

impl Player {
    pub(super) async fn apply_world_change(
        self: &Arc<Self>,
        server: &Server,
        source: &Arc<World>,
        destination: &WorldPosition,
        respawn: Option<RespawnCommit>,
    ) -> bool {
        let (completion, applied) = oneshot::channel();
        server.submit_world_transfer(WorldTransfer {
            player: self.clone(),
            source: source.clone(),
            destination: destination.clone(),
            respawn,
            completion,
        });
        let Ok(Some(change)) = applied.await else {
            return false;
        };
        if let AppliedWorldChange::Moved(mut departure) = change {
            self.finish_world_departure(
                &departure.source,
                std::mem::take(&mut departure.unwatched),
            )
            .await;
        }
        !self.client.closed()
    }

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
        !self.client.closed()
            && self.world_change.load() != WorldChange::Arriving
            && std::ptr::eq(self.living_entity.entity.world.load().as_ref(), world)
    }

    pub(crate) fn can_simulate_in(&self, world: &World) -> bool {
        self.initial_spawn_complete.load(Ordering::Acquire) && self.can_tick_in(world)
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
        let wait = self
            .world()
            .server
            .upgrade()
            .as_deref()
            .is_some_and(can_wait_for_world_change);
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
