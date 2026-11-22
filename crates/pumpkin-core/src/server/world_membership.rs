use std::sync::{Arc, PoisonError};

use pumpkin_data::translation;
use pumpkin_util::math::vector2::Vector2;
use pumpkin_util::math::vector3::Vector3;
use pumpkin_util::text::TextComponent;
use tokio::sync::oneshot;

use crate::entity::EntityBase;
use crate::entity::player::Player;
use crate::entity::player::world_transfer::{
    ArrivalPhase, WorldTransfer, WorldTransferReservation,
};
use crate::net::DisconnectReason;
use crate::world::World;

use super::Server;
use super::player_inventory_edits::PlayerInventoryEdit;

pub(super) enum JoinRejection {
    Unavailable,
    Duplicate,
}

impl JoinRejection {
    pub(super) fn disconnect(self) -> (DisconnectReason, TextComponent) {
        match self {
            Self::Unavailable => (
                DisconnectReason::Shutdown,
                TextComponent::text("Server stopped"),
            ),
            Self::Duplicate => (
                DisconnectReason::LoggedInOtherLocation,
                TextComponent::translate_cross(
                    translation::java::MULTIPLAYER_DISCONNECT_DUPLICATE_LOGIN,
                    translation::bedrock::DISCONNECTIONSCREEN_LOGGEDINOTHERLOCATION,
                    [],
                ),
            ),
        }
    }
}

pub(super) type DeparturePhase = Box<dyn FnOnce(&Server, &Arc<World>) + Send>;

pub(super) enum PlayerAdmission {
    Registered,
    LoadSpawnChunk(Vector2<i32>),
}

pub(super) enum WorldOperation {
    Join {
        world: Arc<World>,
        player: Arc<Player>,
        initial_position: Option<Vector3<f64>>,
        completion: oneshot::Sender<Result<PlayerAdmission, JoinRejection>>,
    },
    PublishInitial {
        world: Arc<World>,
        player: Arc<Player>,
        completion: oneshot::Sender<bool>,
    },
    Transfer(Box<WorldTransfer>),
    Arrival {
        world: Arc<World>,
        player: Arc<Player>,
        phase: ArrivalPhase,
        completion: oneshot::Sender<bool>,
    },
    InventoryEdit(Box<PlayerInventoryEdit>),
    Depart {
        world: Arc<World>,
        phase: DeparturePhase,
    },
}

impl WorldOperation {
    pub(super) fn abandoned(&self) -> bool {
        match self {
            Self::Join {
                completion, player, ..
            } => completion.is_closed() || player.client.closed(),
            Self::PublishInitial {
                completion, player, ..
            }
            | Self::Arrival {
                completion, player, ..
            } => completion.is_closed() || player.client.closed(),
            Self::Transfer(transfer) => transfer.abandoned(),
            Self::Depart { .. } | Self::InventoryEdit(_) => false,
        }
    }

    pub(super) fn try_reserve_worlds(&self) -> Option<WorldTransferReservation> {
        match self {
            Self::Join { world, .. }
            | Self::PublishInitial { world, .. }
            | Self::Arrival { world, .. }
            | Self::Depart { world, .. } => world
                .try_simulation()
                .map(WorldTransferReservation::for_one_world),
            Self::Transfer(transfer) => transfer.try_reserve_worlds(),
            Self::InventoryEdit(edit) => edit
                .world
                .try_simulation()
                .map(WorldTransferReservation::for_one_world),
        }
    }

    pub(super) fn apply(self, server: &Server, reservation: WorldTransferReservation) {
        match self {
            Self::Join {
                world,
                player,
                initial_position,
                completion,
            } => {
                let _owner = reservation;
                if !completion.is_closed() {
                    let result = server.commit_player_admission(&world, &player, initial_position);
                    let registered = matches!(result, Ok(PlayerAdmission::Registered));
                    if completion.send(result).is_err() && registered {
                        world.detach_player(&player);
                    }
                }
            }
            Self::Transfer(transfer) => transfer.apply(server, reservation),
            Self::PublishInitial {
                world,
                player,
                completion,
            } => {
                let _owner = reservation;
                let published = !completion.is_closed()
                    && !server.stop_token.is_cancelled()
                    && world.commit_initial_player(&player);
                let _ = completion.send(published);
            }
            Self::Arrival {
                world,
                player,
                phase,
                completion,
            } => {
                let _owner = reservation;
                let applied = !completion.is_closed()
                    && !server.stop_token.is_cancelled()
                    && player.apply_arrival_phase(server, &world, phase);
                let _ = completion.send(applied);
            }
            Self::Depart { world, phase } => {
                let _owner = reservation;
                phase(server, &world);
            }
            Self::InventoryEdit(edit) => {
                let _owner = reservation;
                edit.apply(server);
            }
        }
    }
}

impl Server {
    pub(crate) async fn publish_initial_player(&self, world: &World, player: &Arc<Player>) -> bool {
        let owner = player.world();
        if !std::ptr::eq(owner.as_ref(), world) {
            return false;
        }
        let (completion, published) = oneshot::channel();
        self.submit_world_operation(WorldOperation::PublishInitial {
            world: owner,
            player: player.clone(),
            completion,
        });
        published.await.unwrap_or(false)
    }

    pub(crate) async fn run_player_arrival(
        &self,
        world: &Arc<World>,
        player: &Arc<Player>,
        phase: ArrivalPhase,
    ) -> bool {
        let (completion, applied) = oneshot::channel();
        self.submit_world_operation(WorldOperation::Arrival {
            world: world.clone(),
            player: player.clone(),
            phase,
            completion,
        });
        applied.await.unwrap_or(false)
    }

    pub(super) async fn request_player_admission(
        &self,
        world: &Arc<World>,
        player: &Arc<Player>,
        initial_position: Option<Vector3<f64>>,
    ) -> Result<(), JoinRejection> {
        loop {
            let (completion, committed) = oneshot::channel();
            self.submit_world_operation(WorldOperation::Join {
                world: world.clone(),
                player: player.clone(),
                initial_position,
                completion,
            });
            match committed.await.unwrap_or(Err(JoinRejection::Unavailable))? {
                PlayerAdmission::Registered => return Ok(()),
                PlayerAdmission::LoadSpawnChunk(chunk) => {
                    // Generation can wait for I/O and workers. The owner rechecks the
                    // current spawn and active session when the chunk is available.
                    world.level.get_or_fetch_chunk(chunk, |_| ()).await;
                }
            }
        }
    }

    fn commit_player_admission(
        &self,
        world: &Arc<World>,
        player: &Arc<Player>,
        initial_position: Option<Vector3<f64>>,
    ) -> Result<PlayerAdmission, JoinRejection> {
        let closed = self
            .admission_closed
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let worlds = self.worlds.load();
        if *closed
            || self.stop_token.is_cancelled()
            || player.client.closed()
            || !Arc::ptr_eq(world, &player.world())
            || !worlds.iter().any(|online| Arc::ptr_eq(online, world))
            || !world.is_open()
        {
            return Err(JoinRejection::Unavailable);
        }
        // A source/destination handoff shares this gate, so the scan sees either membership.
        if worlds.iter().any(|world| {
            world.players.load().iter().any(|online| {
                online.gameprofile.id == player.gameprofile.id
                    || online
                        .gameprofile
                        .name
                        .eq_ignore_ascii_case(&player.gameprofile.name)
            })
        }) {
            return Err(JoinRejection::Duplicate);
        }
        // A login listener can position the new player before admission. Keep
        // that decision as well as positions restored from a player file.
        if let Some(initial_position) = initial_position
            && !player
                .has_played_before
                .load(std::sync::atomic::Ordering::Relaxed)
            && initial_position == player.position()
        {
            match world.initial_player_spawn() {
                Ok(spawn) => spawn.apply(player),
                Err(chunk) => return Ok(PlayerAdmission::LoadSpawnChunk(chunk)),
            }
        }
        let entity = player.get_entity();
        entity.last_pos.store(entity.pos.load());
        world
            .add_player(player)
            .map_err(|_| JoinRejection::Unavailable)?;
        Ok(PlayerAdmission::Registered)
    }
}
