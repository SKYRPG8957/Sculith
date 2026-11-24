//! Coordinates the tick snapshot and durable writer for an explicit save.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};

use pumpkin_world::{
    chunk::io::converter::RegionFormat,
    data::{
        checkpoint::{CheckpointFence, SaveCheckpoint, write_terrain},
        player_data::PlayerDataStorage,
    },
    level::{ChunkSaver, LevelFolder},
};
use rustc_hash::FxHashSet;
use sculith_config::chunk::ChunkConfig;
use sculith_util::math::vector2::Vector2;
use tokio::sync::{OwnedRwLockWriteGuard, oneshot};
use tracing::info;

use crate::{data::player_server::ServerPlayerData, world::World};

use super::{Server, transfer_checkpoint::TransferCheckpoints};

struct Capture {
    checkpoint: SaveCheckpoint,
    access: OwnedRwLockWriteGuard<()>,
}

pub(super) type SavedChunks = BTreeMap<PathBuf, FxHashSet<Vector2<i32>>>;

#[derive(Default)]
pub(super) struct StorageCheckpoint {
    request: Mutex<Option<oneshot::Sender<Result<Capture, String>>>>,
    pub(super) saves: tokio::sync::Mutex<()>,
    // Keeps regional item creation out of the interval between the terrain and
    // entity captures of an explicit save.
    pub(super) regional_entity_changes: tokio::sync::RwLock<()>,
    pub(super) transfers: TransferCheckpoints,
}

impl StorageCheckpoint {
    pub(super) fn capture_at_tick_boundary(
        &self,
        server: &Arc<Server>,
        reserve_boundary: impl FnOnce(&CheckpointFence) -> Option<OwnedRwLockWriteGuard<()>>,
    ) {
        self.transfers.finish_tick(server);
        if !self.manual_capture_requested() {
            return;
        }
        let Some(access) = reserve_boundary(&server.player_data_storage.checkpoint) else {
            return;
        };
        let request = self
            .request
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(send) = request else {
            return;
        };
        if !send.is_closed() {
            let result = capture(server).map(|checkpoint| Capture { checkpoint, access });
            let _ = send.send(result);
        }
    }

    pub(super) fn manual_capture_requested(&self) -> bool {
        self.request
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    pub(super) async fn save(&self, server: &Server) -> Result<Option<SavedChunks>, String> {
        let root = server.basic_config.get_world_path();
        let config = &server.advanced_config.world.chunk;
        let format = RegionFormat::from_config(config);
        let players = server.player_data_storage.checkpoint_storage();
        let fence = &server.player_data_storage.checkpoint;

        if fence.is_pending() {
            let _access = fence.write().await;
            if let Some(checkpoint) = self.transfers.take_retry() {
                self.transfers.write(server, checkpoint).await?;
            } else if let Some(checkpoint) = read(root.clone(), format).await? {
                apply_live(server, Arc::new(checkpoint)).await?;
            } else {
                fence.terrain_materialized();
            }
            fence.set_pending(false);
        }

        if !players.is_save_enabled() {
            return Ok(None);
        }
        let (send, receive) = oneshot::channel();
        *self.request.lock().unwrap_or_else(PoisonError::into_inner) = Some(send);
        let Capture { checkpoint, access } = tokio::select! {
            captured = receive => captured.map_err(|_| "Checkpoint capture stopped".to_string())??,
            () = server.stop_token.cancelled() => return Err("Server stopped before the checkpoint".to_string()),
        };
        if checkpoint.worlds.is_empty() || checkpoint.players.is_empty() {
            return Ok(None);
        }
        // The checkpoint owns both writers until publication and replay finish. On failure,
        // keep the journal authoritative until a retry applies it.
        fence.set_pending(true);
        let covered = checkpoint
            .worlds
            .iter()
            .map(|(relative, chunks)| {
                (
                    root.join(relative),
                    chunks.iter().map(|(pos, _)| *pos).collect(),
                )
            })
            .collect();
        persist(server, Arc::new(checkpoint)).await?;
        fence.set_pending(false);
        drop(access);
        Ok(Some(covered))
    }
}

pub(super) async fn persist(
    server: &Server,
    checkpoint: Arc<SaveCheckpoint>,
) -> Result<(), String> {
    let root = server.basic_config.get_world_path();
    let config = &server.advanced_config.world.chunk;
    let publication_root = root.clone();
    let format = RegionFormat::from_config(config);
    let data = checkpoint.clone();
    blocking(move || {
        data.publish(&publication_root, format)
            .map_err(|err| err.to_string())
    })
    .await?;
    apply_live(server, checkpoint).await
}

async fn apply_live(server: &Server, checkpoint: Arc<SaveCheckpoint>) -> Result<(), String> {
    let fence = &server.player_data_storage.checkpoint;
    let _access = fence.materialize_terrain().await;
    let result = apply(
        checkpoint,
        &server.basic_config.get_world_path(),
        &server.advanced_config.world.chunk,
        &server.worlds.load_full(),
        server.player_data_storage.checkpoint_storage(),
    )
    .await;
    if result.is_ok() {
        fence.terrain_materialized();
    }
    result
}

fn capture(server: &Server) -> Result<SaveCheckpoint, String> {
    let root = server.basic_config.get_world_path();
    let players = ServerPlayerData::capture_online(server);
    let mut checkpoint = SaveCheckpoint {
        players,
        ..SaveCheckpoint::default()
    };
    if checkpoint.players.is_empty() {
        return Ok(checkpoint);
    }
    for world in server.worlds.load().iter() {
        let chunks = world.capture_storage_chunks();
        if !chunks.is_empty() {
            let path = world
                .level
                .level_folder
                .dim_folder
                .strip_prefix(&root)
                .map_err(|err| err.to_string())?;
            checkpoint.worlds.insert(path.to_path_buf(), chunks);
        }
    }
    Ok(checkpoint)
}

pub(super) async fn recover(
    root: &Path,
    config: &ChunkConfig,
    players: Arc<PlayerDataStorage>,
) -> Result<(), String> {
    if let Some(checkpoint) = read(root.to_path_buf(), RegionFormat::from_config(config)).await? {
        let revision = checkpoint.revision;
        apply(Arc::new(checkpoint), root, config, &[], players).await?;
        info!(
            "Recovered world checkpoint revision {revision} in {}",
            root.display()
        );
    }
    Ok(())
}

async fn read(root: PathBuf, format: RegionFormat) -> Result<Option<SaveCheckpoint>, String> {
    blocking(move || SaveCheckpoint::read(&root, format).map_err(|err| err.to_string())).await
}

async fn apply(
    checkpoint: Arc<SaveCheckpoint>,
    root: &Path,
    config: &ChunkConfig,
    worlds: &[Arc<World>],
    players: Arc<PlayerDataStorage>,
) -> Result<(), String> {
    let target = root.to_path_buf();
    let (checkpoint, terrain) = blocking(move || {
        SaveCheckpoint::confirm_publication(&target).map_err(|err| err.to_string())?;
        let terrain = checkpoint.prepare_terrain()?;
        Ok((checkpoint, terrain))
    })
    .await?;
    for (relative, chunks) in terrain {
        let path = root.join(relative);
        if let Some(world) = worlds
            .iter()
            .find(|world| world.level.level_folder.dim_folder == path)
        {
            write_terrain(&world.level.chunk_saver, &world.level.level_folder, chunks).await?;
        } else {
            let folder = LevelFolder::new(root.to_path_buf(), path);
            write_terrain(&ChunkSaver::from_config(config), &folder, chunks).await?;
        }
    }
    let root = root.to_path_buf();
    blocking(move || {
        checkpoint.save_players(&players)?;
        SaveCheckpoint::remove(&root).map_err(|err| err.to_string())
    })
    .await
}

async fn blocking<T: Send + 'static>(
    action: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(action)
        .await
        .map_err(|err| io::Error::other(err).to_string())?
}
