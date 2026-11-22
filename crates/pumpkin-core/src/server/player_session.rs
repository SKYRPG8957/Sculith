use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use rustc_hash::FxHashSet;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::data::player_server::ServerPlayerData;
use crate::entity::player::Player;
use crate::net::{ClientPlatform, GameProfile, PlayerConfig};
use crate::world::World;

use super::Server;
use super::world_membership::{JoinRejection, WorldMembershipChange};

/// Keeps the same account from loading data while its previous connection is saving.
pub struct NetworkPlayerSession {
    uuid: Uuid,
    sessions: Arc<Mutex<FxHashSet<Uuid>>>,
}

impl Drop for NetworkPlayerSession {
    fn drop(&mut self) {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.uuid);
    }
}

struct DepartedWorld(Arc<World>);

impl DepartedWorld {
    fn pin(world: &Arc<World>) -> Self {
        world.pending_operations.fetch_add(1, Ordering::AcqRel);
        Self(world.clone())
    }
}

impl Drop for DepartedWorld {
    fn drop(&mut self) {
        self.0.pending_operations.fetch_sub(1, Ordering::Release);
    }
}

impl Server {
    pub(crate) async fn add_network_player(
        self: &Arc<Self>,
        client: Arc<ClientPlatform>,
        profile: GameProfile,
        config: PlayerConfig,
    ) -> Option<(Arc<Player>, Arc<World>, NetworkPlayerSession)> {
        let uuid = profile.id;
        let inserted = self
            .network_sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(uuid);
        if !inserted {
            let (reason, message) = JoinRejection::Duplicate.disconnect();
            client.try_kick(reason, &message);
            return None;
        }
        let session = NetworkPlayerSession {
            uuid,
            sessions: self.network_sessions.clone(),
        };
        let (player, world) = self.add_player(client, profile, Some(config)).await?;
        Some((player, world, session))
    }

    pub(crate) async fn finish_player_connection(self: &Arc<Self>, player: &Arc<Player>) {
        let departure = loop {
            let world = player.world();
            let session = player.clone();
            let Some(detached) = self
                .run_departure_phase(world.clone(), move |_, world| {
                    if !Arc::ptr_eq(&session.world(), world)
                        || !world
                            .players
                            .load()
                            .iter()
                            .any(|member| Arc::ptr_eq(member, &session))
                    {
                        return None;
                    }
                    let departure = DepartedWorld::pin(world);
                    session.prepare_world_departure();
                    if !world.detach_player(&session) {
                        return None;
                    }
                    world.notify_player_departure(&session);
                    Some(departure)
                })
                .await
            else {
                return;
            };
            if let Some(departure) = detached {
                break departure;
            }
            if Arc::ptr_eq(&player.world(), &world) {
                return;
            }
        };

        // Leave listeners can wait for other world work while this world stays loaded.
        let world = &departure.0;
        world.send_player_leave(player).await;
        let session = player.clone();
        let Some(snapshot) = self
            .run_departure_phase(world.clone(), move |server, world| {
                session.release_departure_world(world);
                server.remove_player(&session);
                ServerPlayerData::close_leave_inventory(&session);
                server
                    .player_data_storage
                    .is_save_enabled()
                    .then(|| ServerPlayerData::capture(&session))
            })
            .await
        else {
            return;
        };
        drop(departure);

        if let Some(snapshot) = snapshot
            && let Err(error) = self
                .player_data_storage
                .save_player_snapshot(|| snapshot)
                .await
        {
            tracing::error!("Failed to save player data on disconnect: {error}");
        }
        if player.client.java().is_some()
            && let Err(error) = self.advancement_manager.save_player(player).await
        {
            tracing::error!("Failed to save player advancement on disconnect: {error}");
        }
    }

    async fn run_departure_phase<T: Send + 'static>(
        self: &Arc<Self>,
        world: Arc<World>,
        action: impl FnOnce(&Self, &Arc<World>) -> T + Send + 'static,
    ) -> Option<T> {
        let work = Arc::new(Mutex::new(Some(action)));
        let scheduled = work.clone();
        let (done, result) = oneshot::channel();
        self.submit_membership_change(WorldMembershipChange::Depart {
            world: world.clone(),
            phase: Box::new(move |server, world| {
                let action = scheduled
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                if let Some(action) = action {
                    let _ = done.send(action(server, world));
                }
            }),
        });
        if let Ok(result) = result.await {
            return Some(result);
        }

        // Shutdown discards queued jobs before waiting for active owners. Take only work
        // that never started, and reserve this world even after the ticker has stopped.
        let action = work.lock().unwrap_or_else(PoisonError::into_inner).take()?;
        let owner = world.simulation.clone().lock_owned().await;
        let server = self.clone();
        self.runtime
            .spawn_blocking(move || {
                let _owner = owner;
                action(&server, &world)
            })
            .await
            .map_err(|error| tracing::error!("Player departure failed during shutdown: {error}"))
            .ok()
    }
}
