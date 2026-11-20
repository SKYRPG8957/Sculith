//! Batches direct transfers at the simulation boundary under the shared storage fence.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError, atomic::Ordering},
    time::{Duration, Instant},
};

use pumpkin_inventory::{
    generic_container_screen_handler::GenericContainerScreenHandler, screen_handler::ScreenHandler,
};
use pumpkin_util::math::vector2::Vector2;
use pumpkin_world::{
    data::checkpoint::{CheckpointFence, SaveCheckpoint},
    level::SyncChunk,
};
use rustc_hash::FxHashMap;
use tokio::sync::OwnedRwLockWriteGuard;
use tracing::{debug, error};
use uuid::Uuid;

use crate::{
    block::entities::BlockEntity, data::player_server::ServerPlayerData, entity::player::Player,
    world::World,
};

use super::{Server, storage_checkpoint::persist};

/// The tick can collect 100 separate player-to-container pairs with room for more targets.
const MAX_TARGETS: usize = 256;
const RETRY_INTERVAL: Duration = Duration::from_secs(1);

pub struct StorageTransfer {
    world: Arc<World>,
    chunk: SyncChunk,
    entity: Arc<dyn BlockEntity>,
}

impl StorageTransfer {
    pub(crate) fn record(self) {
        self.world
            .record_storage_transfer(&self.chunk, self.entity.as_ref());
        if let Some(server) = self.world.server.upgrade() {
            server.storage_checkpoint.transfers.mark_changed();
        }
    }
}

impl Server {
    pub(crate) fn prepare_storage_transfer(
        &self,
        player: &Arc<Player>,
        handler: &dyn ScreenHandler,
    ) -> Result<Option<StorageTransfer>, ()> {
        let world = player.world();
        if !self.player_data_storage.is_save_enabled()
            || !world.level.save_enabled.load(Ordering::Relaxed)
        {
            return Ok(None);
        }
        let Some(storage) = handler
            .as_any()
            .downcast_ref::<GenericContainerScreenHandler>()
        else {
            return Ok(None);
        };
        let Some(pos) = player.open_container_pos.load() else {
            return Ok(None);
        };
        let Some(entity) = world.get_block_entity(&pos) else {
            return Ok(None);
        };
        if !matches!(
            entity.resource_location(),
            "minecraft:chest" | "minecraft:trapped_chest" | "minecraft:barrel"
        ) || entity
            .clone()
            .get_inventory()
            .is_none_or(|inventory| !Arc::ptr_eq(&inventory, &storage.inventory))
        {
            return Ok(None);
        }

        let chunk = world
            .level
            .loaded_chunks
            .get(&pos.chunk_position())
            .ok_or(())?
            .value()
            .clone();
        let root = self.basic_config.get_world_path();
        let relative = world
            .level
            .level_folder
            .dim_folder
            .strip_prefix(root)
            .map_err(|_| ())?
            .to_path_buf();
        if !self.storage_checkpoint.transfers.admit(
            &self.player_data_storage.checkpoint,
            !self.storage_checkpoint.manual_capture_requested(),
            player,
            world.clone(),
            relative,
            chunk.clone(),
        ) {
            return Err(());
        }
        Ok(Some(StorageTransfer {
            world,
            chunk,
            entity,
        }))
    }
}

struct Collecting {
    access: OwnedRwLockWriteGuard<()>,
    changed: bool,
    players: FxHashMap<Uuid, Arc<Player>>,
    terrain: FxHashMap<(PathBuf, Vector2<i32>), (Arc<World>, SyncChunk)>,
}

#[derive(Default)]
enum Phase {
    #[default]
    Idle,
    Collecting(Collecting),
    Writing(u64),
    Retry {
        data: Arc<SaveCheckpoint>,
        next_attempt: Instant,
    },
}

#[derive(Default)]
struct State {
    phase: Phase,
    revision: u64,
}

#[derive(Default)]
pub(super) struct TransferCheckpoints {
    state: Mutex<State>,
}

impl TransferCheckpoints {
    pub(super) fn admit(
        &self,
        fence: &CheckpointFence,
        start_allowed: bool,
        player: &Arc<Player>,
        world: Arc<World>,
        relative: PathBuf,
        chunk: SyncChunk,
    ) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(state.phase, Phase::Idle) && start_allowed && !fence.is_pending() {
            let Some(access) = fence.try_write() else {
                return false;
            };
            fence.set_pending(true);
            state.phase = Phase::Collecting(Collecting {
                access,
                changed: false,
                players: FxHashMap::default(),
                terrain: FxHashMap::default(),
            });
        }
        let Phase::Collecting(batch) = &mut state.phase else {
            return false;
        };
        let id = player.gameprofile.id;
        let key = (relative, Vector2::new(chunk.x, chunk.z));
        let new_targets = usize::from(!batch.players.contains_key(&id))
            + usize::from(!batch.terrain.contains_key(&key));
        if batch.players.len() + batch.terrain.len() + new_targets > MAX_TARGETS {
            return false;
        }
        batch.players.entry(id).or_insert_with(|| player.clone());
        batch.terrain.entry(key).or_insert((world, chunk));
        true
    }

    fn mark_changed(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Phase::Collecting(batch) = &mut state.phase {
            batch.changed = true;
        }
    }

    pub(super) fn finish_tick(&self, server: &Arc<Server>) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let fence = &server.player_data_storage.checkpoint;
        let (data, access) = match &mut state.phase {
            Phase::Collecting(batch) if !batch.changed => {
                fence.set_pending(false);
                state.phase = Phase::Idle;
                return;
            }
            Phase::Collecting(_) => {
                state.revision += 1;
                let revision = state.revision;
                let previous = std::mem::replace(&mut state.phase, Phase::Writing(revision));
                let Phase::Collecting(batch) = previous else {
                    state.phase = previous;
                    return;
                };
                drop(state);
                batch.capture(revision)
            }
            Phase::Retry { data, next_attempt } if Instant::now() >= *next_attempt => {
                let Some(access) = fence.try_write() else {
                    return;
                };
                let data = data.clone();
                state.phase = Phase::Writing(data.revision);
                (data, access)
            }
            _ => return,
        };

        let task = server.clone();
        server.spawn_task(async move {
            let _access = access;
            let _ = task.storage_checkpoint.transfers.write(&task, data).await;
        });
    }

    /// A manual save can retry the same image immediately after acquiring the storage fence.
    pub(super) fn take_retry(&self) -> Option<Arc<SaveCheckpoint>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let Phase::Retry { data, .. } = &state.phase else {
            return None;
        };
        let data = data.clone();
        state.phase = Phase::Writing(data.revision);
        Some(data)
    }

    pub(super) async fn write(
        &self,
        server: &Server,
        data: Arc<SaveCheckpoint>,
    ) -> Result<(), String> {
        let result = persist(server, data.clone()).await;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        debug_assert!(matches!(state.phase, Phase::Writing(revision) if revision == data.revision));
        match &result {
            Ok(()) => {
                debug!("Inventory checkpoint {} is durable", data.revision);
                state.phase = Phase::Idle;
                server.player_data_storage.checkpoint.set_pending(false);
            }
            Err(err) => {
                error!("Inventory checkpoint {} failed: {err}", data.revision);
                state.phase = Phase::Retry {
                    data,
                    next_attempt: Instant::now() + RETRY_INTERVAL,
                };
            }
        }
        result
    }
}

impl Collecting {
    fn capture(self, revision: u64) -> (Arc<SaveCheckpoint>, OwnedRwLockWriteGuard<()>) {
        let mut data = SaveCheckpoint {
            revision,
            players: self
                .players
                .values()
                .map(|player| ServerPlayerData::capture(player))
                .collect(),
            ..SaveCheckpoint::default()
        };
        for ((path, pos), (world, chunk)) in self.terrain {
            data.worlds
                .entry(path)
                .or_default()
                .push((pos, world.capture_storage_chunk(&chunk)));
        }
        (Arc::new(data), self.access)
    }
}
