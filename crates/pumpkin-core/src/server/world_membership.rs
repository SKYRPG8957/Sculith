use std::sync::{Arc, PoisonError};

use pumpkin_data::translation;
use pumpkin_util::text::TextComponent;
use tokio::sync::oneshot;

use crate::entity::player::Player;
use crate::entity::player::world_transfer::{WorldTransfer, WorldTransferReservation};
use crate::net::DisconnectReason;
use crate::world::World;

use super::Server;

pub(super) enum JoinRejection {
    Unavailable,
    Duplicate,
}

impl JoinRejection {
    pub(super) fn kick(self, player: &Player) {
        match self {
            Self::Unavailable => {
                player.kick(
                    DisconnectReason::Shutdown,
                    &TextComponent::text("Server stopped"),
                );
            }
            Self::Duplicate => player.kick(
                DisconnectReason::LoggedInOtherLocation,
                &TextComponent::translate_cross(
                    translation::java::MULTIPLAYER_DISCONNECT_DUPLICATE_LOGIN,
                    translation::bedrock::DISCONNECTIONSCREEN_LOGGEDINOTHERLOCATION,
                    [],
                ),
            ),
        }
    }
}

pub(super) enum WorldMembershipChange {
    Join {
        world: Arc<World>,
        player: Arc<Player>,
        completion: oneshot::Sender<Result<(), JoinRejection>>,
    },
    Transfer(Box<WorldTransfer>),
}

impl WorldMembershipChange {
    pub(super) fn abandoned(&self) -> bool {
        matches!(self, Self::Join { completion, player, .. } if completion.is_closed() || player.client.closed())
    }

    pub(super) fn try_reserve_worlds(&self) -> Option<WorldTransferReservation> {
        match self {
            Self::Join { world, .. } => world
                .try_simulation()
                .map(WorldTransferReservation::for_one_world),
            Self::Transfer(transfer) => transfer.try_reserve_worlds(),
        }
    }

    pub(super) fn apply(self, server: &Server, reservation: WorldTransferReservation) {
        match self {
            Self::Join {
                world,
                player,
                completion,
            } => {
                let _owner = reservation;
                if !completion.is_closed() {
                    let result = server.commit_player_admission(&world, &player);
                    let registered = result.is_ok();
                    if completion.send(result).is_err() && registered {
                        world.detach_player(&player);
                    }
                }
            }
            Self::Transfer(transfer) => transfer.apply(server, reservation),
        }
    }
}

impl Server {
    pub(super) async fn request_player_admission(
        &self,
        world: &Arc<World>,
        player: &Arc<Player>,
    ) -> Result<(), JoinRejection> {
        let (completion, committed) = oneshot::channel();
        self.submit_membership_change(WorldMembershipChange::Join {
            world: world.clone(),
            player: player.clone(),
            completion,
        });
        committed.await.unwrap_or(Err(JoinRejection::Unavailable))
    }

    fn commit_player_admission(
        &self,
        world: &Arc<World>,
        player: &Arc<Player>,
    ) -> Result<(), JoinRejection> {
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
        world
            .add_player(player)
            .map_err(|_| JoinRejection::Unavailable)
    }
}
