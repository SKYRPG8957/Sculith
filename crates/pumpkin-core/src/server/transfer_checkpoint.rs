//! Batches inventory transfers at the simulation boundary under the shared storage fence.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError, atomic::Ordering},
    time::{Duration, Instant},
};

use pumpkin_inventory::{
    Inventory, double::DoubleInventory,
    generic_container_screen_handler::GenericContainerScreenHandler, screen_handler::ScreenHandler,
};
use pumpkin_util::math::position::BlockPos;
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
    block::{blocks::chests::paired_chest_for_inventory, entities::BlockEntity},
    data::player_server::ServerPlayerData,
    entity::player::Player,
    world::World,
};

use super::{Server, storage_checkpoint::persist};

/// Bound the combined number of players and terrain chunks captured in a tick.
const MAX_TARGETS: usize = 256;
const RETRY_INTERVAL: Duration = Duration::from_secs(1);

pub enum StorageTransferError {
    Busy,
    StaleContainer,
}

struct TerrainTarget {
    chunk: SyncChunk,
    entity: Arc<dyn BlockEntity>,
}

impl TerrainTarget {
    fn loaded(world: &World, entity: Arc<dyn BlockEntity>) -> Result<Self, StorageTransferError> {
        let chunk = world
            .level
            .loaded_chunks
            .get(&entity.get_position().chunk_position())
            .ok_or(StorageTransferError::StaleContainer)?
            .value()
            .clone();
        Ok(Self { chunk, entity })
    }

    fn position(&self) -> Vector2<i32> {
        Vector2::new(self.chunk.x, self.chunk.z)
    }
}

pub struct StorageTransfer {
    world: Arc<World>,
    primary: TerrainTarget,
    related: [Option<TerrainTarget>; 2],
}

impl StorageTransfer {
    fn targets(&self) -> impl Iterator<Item = &TerrainTarget> + Clone {
        std::iter::once(&self.primary).chain(self.related.iter().flatten())
    }

    pub(crate) fn record(self) {
        for target in self.targets() {
            self.world
                .record_storage_transfer(&target.chunk, target.entity.as_ref());
        }
        if let Some(server) = self.world.server.upgrade() {
            server.storage_checkpoint.transfers.mark_changed();
        }
    }
}

struct BlockInventoryMembers {
    entity: Arc<dyn BlockEntity>,
    paired: Option<Arc<dyn BlockEntity>>,
}

/// Resolve a block inventory back to the live entities that own its slots.
fn live_block_inventory(
    world: &World,
    pos: &BlockPos,
    inventory: &Arc<dyn Inventory>,
) -> Option<BlockInventoryMembers> {
    let primary = world.get_block_entity(pos)?;
    let paired = if let Some(double) = inventory.as_any().downcast_ref::<DoubleInventory>() {
        Some(paired_chest_for_inventory(world, pos, &primary, double)?)
    } else {
        let backing = primary.clone().get_inventory()?;
        if !Arc::ptr_eq(&backing, inventory) {
            return None;
        }
        None
    };
    Some(BlockInventoryMembers {
        entity: primary,
        paired,
    })
}

fn is_checkpoint_container(entity: &dyn BlockEntity) -> bool {
    matches!(
        entity.resource_location(),
        "minecraft:chest" | "minecraft:trapped_chest" | "minecraft:barrel" | "minecraft:hopper"
    )
}

impl Server {
    pub(crate) fn prepare_storage_transfer(
        &self,
        player: &Arc<Player>,
        handler: &dyn ScreenHandler,
    ) -> Result<Option<StorageTransfer>, StorageTransferError> {
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
        let Some(members) = live_block_inventory(&world, &pos, &storage.inventory) else {
            return if storage.inventory.as_any().is::<DoubleInventory>() {
                Err(StorageTransferError::StaleContainer)
            } else {
                Ok(None)
            };
        };
        if !is_checkpoint_container(members.entity.as_ref()) {
            return Ok(None);
        }
        self.prepare_inventory_checkpoint(
            &world,
            Some(player),
            members.entity,
            [members.paired, None],
        )
        .map(Some)
    }

    pub(crate) fn prepare_hopper_storage_transfer(
        &self,
        world: &Arc<World>,
        hopper: Arc<dyn BlockEntity>,
        other_pos: &BlockPos,
        other_inventory: &Arc<dyn Inventory>,
    ) -> Result<Option<StorageTransfer>, StorageTransferError> {
        if !world.level.save_enabled.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let members = live_block_inventory(world, other_pos, other_inventory)
            .ok_or(StorageTransferError::StaleContainer)?;
        if !is_checkpoint_container(members.entity.as_ref()) {
            return Ok(None);
        }
        self.prepare_inventory_checkpoint(
            world,
            None,
            hopper,
            [Some(members.entity), members.paired],
        )
        .map(Some)
    }

    fn prepare_inventory_checkpoint(
        &self,
        world: &Arc<World>,
        player: Option<&Arc<Player>>,
        primary: Arc<dyn BlockEntity>,
        [second, third]: [Option<Arc<dyn BlockEntity>>; 2],
    ) -> Result<StorageTransfer, StorageTransferError> {
        let root = self.basic_config.get_world_path();
        let relative = world
            .level
            .level_folder
            .dim_folder
            .strip_prefix(root)
            .map_err(|_| StorageTransferError::Busy)?
            .to_path_buf();
        let transfer = StorageTransfer {
            primary: TerrainTarget::loaded(world, primary)?,
            related: [
                second
                    .map(|entity| TerrainTarget::loaded(world, entity))
                    .transpose()?,
                third
                    .map(|entity| TerrainTarget::loaded(world, entity))
                    .transpose()?,
            ],
            world: world.clone(),
        };
        if !self.storage_checkpoint.transfers.admit(
            &self.player_data_storage.checkpoint,
            !self.storage_checkpoint.manual_capture_requested(),
            player,
            &relative,
            &transfer,
        ) {
            return Err(StorageTransferError::Busy);
        }
        Ok(transfer)
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
        player: Option<&Arc<Player>>,
        relative: &Path,
        transfer: &StorageTransfer,
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
        let targets = transfer.targets();
        let chunks = targets
            .clone()
            .enumerate()
            .filter_map(move |(index, target)| {
                let position = target.position();
                let already_counted = targets
                    .clone()
                    .take(index)
                    .any(|prior| prior.position() == position);
                (!already_counted).then(|| ((relative.to_path_buf(), position), &target.chunk))
            });
        let new_player =
            player.is_some_and(|player| !batch.players.contains_key(&player.gameprofile.id));
        let new_targets = usize::from(new_player)
            + chunks
                .clone()
                .filter(|(key, _)| !batch.terrain.contains_key(key))
                .count();
        if batch.players.len() + batch.terrain.len() + new_targets > MAX_TARGETS {
            return false;
        }
        if let Some(player) = player {
            batch
                .players
                .entry(player.gameprofile.id)
                .or_insert_with(|| player.clone());
        }
        for (key, chunk) in chunks {
            batch
                .terrain
                .entry(key)
                .or_insert_with(|| (transfer.world.clone(), chunk.clone()));
        }
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
