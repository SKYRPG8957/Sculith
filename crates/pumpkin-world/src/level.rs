use crate::chunk_system::{ChunkListener, ChunkLoading, GenerationSchedule, LevelChannel};
use crate::generation::generator::WorldGenerator;
use crate::lighting::DynamicLightEngine;
use crate::{
    chunk::{
        ChunkData, ChunkEntityData, DownwardWaterTick,
        io::{Dirtiable, FileIO, LoadedData, file_manager::LevelFileIO},
        palette::has_random_ticking_fluid,
    },
    data::checkpoint::CheckpointFence,
    generation::get_world_gen_with_all_settings,
    tick::{OrderedTick, ScheduledTick, TickPriority, order},
    world::WorldPortalExt,
};
use arc_swap::ArcSwap;
use crossbeam::queue::SegQueue;
use dashmap::{DashMap, Entry, try_result::TryResult};
use futures::stream::{FuturesUnordered, StreamExt};
use pumpkin_config::{lighting::LightingEngineConfig, world::LevelConfig};
use pumpkin_data::biome::Biome;
use pumpkin_data::dimension::Dimension;
use pumpkin_data::{Block, BlockStateId, block_properties::has_random_ticks, fluid::Fluid};
use rayon::prelude::*;
use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};
use sculith_util::math::{position::BlockPos, vector2::Vector2};
use sculith_util::world_seed::Seed;
use std::collections::hash_map::Entry as HashEntry;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread,
};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};
// use tokio::runtime::Handle;
use tokio::{
    select,
    sync::{
        mpsc::{self, Receiver},
        oneshot,
    },
    task::JoinHandle,
};
use tokio_util::task::TaskTracker;

pub type SyncChunk = Arc<ChunkData>;
pub type SyncEntityChunk = Arc<ChunkEntityData>;

/// Commits ready property changes to the loaded source chunk.
///
/// The caller's region permit must cover every chunk accepted by `owns` until this returns.
/// State reads may extend one chunk in either horizontal direction.
pub fn apply_regional_block_properties(
    loaded: &DashMap<Vector2<i32>, SyncChunk, FxBuildHasher>,
    expected: &SyncChunk,
    limit: usize,
    owns: impl Fn(Vector2<i32>) -> bool,
    update: impl Fn(
        &Block,
        BlockPos,
        BlockStateId,
        &dyn Fn(BlockPos) -> Option<BlockStateId>,
    ) -> Option<BlockStateId>,
) -> Option<Vec<(BlockPos, BlockStateId, BlockStateId)>> {
    apply_regional_chunk_tick(loaded, expected, owns, |chunk, read, can_commit| {
        chunk.try_apply_ready_block_properties(limit, read, can_commit, &update)
    })
}

fn apply_regional_chunk_tick<R>(
    loaded: &DashMap<Vector2<i32>, SyncChunk, FxBuildHasher>,
    expected: &SyncChunk,
    owns: impl Fn(Vector2<i32>) -> bool,
    apply: impl Fn(
        &ChunkData,
        &dyn Fn(BlockPos) -> Option<BlockStateId>,
        &dyn Fn() -> bool,
    ) -> Option<R>,
) -> Option<R> {
    let source = Vector2::new(expected.x, expected.z);
    if !owns(source) {
        return None;
    }
    let TryResult::Present(chunk) = loaded.try_get(&source) else {
        return None;
    };
    if !Arc::ptr_eq(chunk.value(), expected) {
        return None;
    }

    let first_neighbor = std::cell::Cell::new(None);
    let can_commit = || owns(source);
    let local = apply(
        &chunk,
        &|pos| {
            first_neighbor.set(Some(pos.chunk_position()));
            None
        },
        &can_commit,
    );
    if local.is_some() || first_neighbor.get().is_none_or(|pos| !owns(pos)) {
        return local;
    }

    // Retain the loaded-map references as well as the read locks. Acquiring a
    // shard or palette never waits while another palette is held.
    let mut neighbors = Vec::with_capacity(8);
    for dx in -1..=1 {
        for dz in -1..=1 {
            let pos = source.add_raw(dx, dz);
            if pos != source
                && owns(pos)
                && let TryResult::Present(neighbor) = loaded.try_get(&pos)
            {
                neighbors.push(neighbor);
            }
        }
    }
    let palettes: Vec<_> = neighbors
        .iter()
        .filter_map(|chunk| {
            let sections = &chunk.section;
            let palettes = sections.block_sections.try_read()?;
            Some((*chunk.key(), sections, palettes))
        })
        .collect();
    let read_neighbor = |position: BlockPos| {
        let (pos, relative) = position.chunk_and_chunk_relative_position();
        let (_, sections, palettes) = palettes.iter().find(|(chunk, ..)| *chunk == pos)?;
        sections.read_block_from_palettes(palettes, relative)
    };
    apply(&chunk, &read_neighbor, &can_commit)
}

type EntityLoadWaiters = FxHashMap<Vector2<i32>, Vec<oneshot::Sender<SyncEntityChunk>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadedChunkChange {
    Loaded(Vector2<i32>),
    Unloaded(Vector2<i32>),
}

pub type ChunkSaver = LevelFileIO<ChunkData>;

pub type EntitySaver = LevelFileIO<ChunkEntityData>;

pub(crate) type SaveCompletion = oneshot::Sender<Result<(), String>>;

pub(crate) struct SaveRequest {
    pub completion: Option<SaveCompletion>,
    pub checkpoint_chunks: FxHashSet<Vector2<i32>>,
    pub clock: Option<crate::world_info::clock::WorldClockSnapshot>,
}

/// The `Level` module provides functionality for working with chunks within or outside a Minecraft world.
///
/// Key features include:
///
/// - **Chunk Loading:** Efficiently loads chunks from disk.
/// - **Chunk Caching:** Stores accessed chunks in memory for faster access.
/// - **Chunk Generation:** Generates new chunks on-demand using a specified `WorldGenerator`.
///
/// For more details on world generation, refer to the `WorldGenerator` module.
pub struct Level {
    pub seed: Seed,
    pub world_portal: ArcSwap<Option<Arc<dyn WorldPortalExt>>>,
    pub level_folder: Arc<LevelFolder>,
    pub lighting_config: LightingEngineConfig,

    /// Counts the number of ticks that have been scheduled for this world
    schedule_tick_counts: AtomicU64,

    // Chunks that are paired with chunk watchers. When a chunk is no longer watched, it is removed
    // from the loaded chunks map and sent to the underlying ChunkIO
    pub loaded_chunks: Arc<DashMap<Vector2<i32>, SyncChunk, FxBuildHasher>>,
    pub(crate) loaded_chunk_changes: Arc<SegQueue<LoadedChunkChange>>,
    loaded_entity_chunks: Arc<DashMap<Vector2<i32>, SyncEntityChunk, FxBuildHasher>>,
    entity_chunk_handoff: Mutex<EntityLoadWaiters>,
    pub chunks_with_scheduled_ticks: Arc<dashmap::DashSet<Vector2<i32>>>,
    pub chunk_loading: Mutex<ChunkLoading>,

    chunk_watchers: Arc<DashMap<Vector2<i32>, usize>>,

    pub chunk_saver: Arc<ChunkSaver>,
    checkpoint_fence: Arc<CheckpointFence>,
    entity_saver: Arc<EntitySaver>,

    pub world_gen: ArcSwap<WorldGenerator>,

    /// Handles runtime lighting updates
    pub light_engine: DynamicLightEngine,

    /// Tracks tasks associated with this world instance
    tasks: TaskTracker,
    pub rt_handle: OnceLock<tokio::runtime::Handle>,
    pub chunk_system_tasks: TaskTracker,
    /// Notification that interrupts tasks for shutdown
    pub cancel_token: CancellationToken,

    pub shut_down_chunk_system: AtomicBool,
    pub should_save: AtomicBool,
    manual_saves: mpsc::UnboundedSender<SaveRequest>,
    pub should_unload: AtomicBool,
    /// Whether periodic autosaving is enabled. Toggled by `/save-off` and `/save-on`;
    /// a manual `/save-all` still saves while this is `false`.
    pub save_enabled: AtomicBool,
    /// Number of ticks between autosave checks. If 0, autosave is disabled.
    pub autosave_ticks: u64,

    pub level_channel: Arc<LevelChannel>,
    pub thread_tracker: Mutex<Vec<thread::JoinHandle<()>>>,
    pub chunk_listener: Arc<ChunkListener>,
}

#[derive(Default)]
pub struct TickData {
    pub block_ticks: Vec<OrderedTick<&'static Block>>,
    pub fluid_ticks: Vec<OrderedTick<&'static Fluid>>,
    pub random_ticks: Vec<RandomTickSample>,
}

pub struct ScheduledTickGroup {
    pub chunks: Vec<Vector2<i32>>,
    pub step: ScheduledTickStep,
}

#[derive(Clone, Copy)]
pub enum ScheduledTickStep {
    Advance(Option<u64>),
    Ready,
}

#[derive(Clone, Copy)]
pub struct RandomTickSample {
    pub position: BlockPos,
    pub tick_block: bool,
    pub tick_fluid: bool,
}

pub struct LevelFolder {
    pub root_folder: PathBuf,
    pub dim_folder: PathBuf,
    pub region_folder: PathBuf,
    pub entities_folder: PathBuf,
    pub poi_folder: PathBuf,
}

impl LevelFolder {
    #[must_use]
    pub fn new(root_folder: PathBuf, dim_folder: PathBuf) -> Self {
        Self {
            region_folder: dim_folder.join("region"),
            entities_folder: dim_folder.join("entities"),
            poi_folder: dim_folder.join("poi"),
            root_folder,
            dim_folder,
        }
    }

    #[must_use]
    pub fn for_dimension(root_folder: PathBuf, dimension: &Dimension) -> Self {
        let (namespace, name) = dimension
            .minecraft_name
            .split_once(':')
            .unwrap_or(("minecraft", dimension.minecraft_name));
        let canonical = root_folder.join("dimensions").join(namespace).join(name);
        let dim_folder = if canonical.exists() {
            canonical
        } else if dimension.minecraft_name == Dimension::OVERWORLD.minecraft_name
            && root_folder.join("region").exists()
        {
            root_folder.clone()
        } else if dimension.minecraft_name == Dimension::THE_NETHER.minecraft_name
            && root_folder.join("DIM-1").join("region").exists()
        {
            root_folder.join("DIM-1")
        } else if dimension.minecraft_name == Dimension::THE_END.minecraft_name
            && root_folder.join("DIM1").join("region").exists()
        {
            root_folder.join("DIM1")
        } else {
            canonical
        };
        Self::new(root_folder, dim_folder)
    }
}

impl Level {
    #[must_use]
    pub fn from_root_folder(
        level_config: &LevelConfig,
        root_folder: PathBuf,
        seed: i64,
        dimension: Dimension,
    ) -> Arc<Self> {
        Self::with_checkpoint_fence(level_config, root_folder, seed, dimension, Arc::default())
    }

    #[must_use]
    #[expect(clippy::too_many_lines)]
    pub fn with_checkpoint_fence(
        level_config: &LevelConfig,
        root_folder: PathBuf,
        seed: i64,
        dimension: Dimension,
        checkpoint_fence: Arc<CheckpointFence>,
    ) -> Arc<Self> {
        // 26.2 canonical layout: root_folder/dimensions/<namespace>/<name>
        let level_folder = Arc::new(LevelFolder::for_dimension(root_folder, &dimension));
        let _ = std::fs::create_dir_all(&level_folder.region_folder);
        let _ = std::fs::create_dir_all(&level_folder.entities_folder);
        let _ = std::fs::create_dir_all(&level_folder.poi_folder);

        let main_folder = &level_folder.root_folder;

        let mut is_flat = false;
        let mut flat_layers = Vec::new();
        let mut flat_biome = "minecraft:plains".to_string();
        let mut generator_settings_name: Option<String> = None;
        let mut biome_source: Option<crate::world_info::BiomeSource> = None;
        let mut structure_overrides: Option<Vec<String>> = None;

        if let Some(wgs) = crate::world_info::data_files::read_world_gen_settings(main_folder)
            && let Some(dim_settings) = wgs.dimensions.get(dimension.minecraft_name)
        {
            biome_source.clone_from(&dim_settings.generator.biome_source);

            if dim_settings.generator.generator_type == "minecraft:flat" {
                is_flat = true;
                let flat_settings = dim_settings
                    .generator
                    .settings
                    .as_ref()
                    .and_then(crate::world_info::GeneratorSettings::as_flat_settings)
                    .or_else(|| {
                        crate::world_info::FlatLevelGeneratorPreset::from_name("classic_flat")
                            .map(|p| p.settings)
                    });
                if let Some(flat_settings) = flat_settings {
                    flat_layers = flat_settings.to_flat_layers();
                    structure_overrides = flat_settings.structure_overrides_vec();
                    flat_biome = flat_settings.biome;
                }
            } else if let Some(crate::world_info::GeneratorSettings::Reference(s)) =
                &dim_settings.generator.settings
            {
                generator_settings_name = Some(s.clone());
            }
        }

        let dim_min_y = dimension.min_y;
        let dim_height = dimension.height;
        let seed = Seed(seed as u64);
        let world_gen: Arc<WorldGenerator> = Arc::from(get_world_gen_with_all_settings(
            seed,
            dimension,
            is_flat,
            flat_layers,
            flat_biome,
            generator_settings_name.as_deref(),
            biome_source.as_ref(),
            structure_overrides.as_deref(),
        ));
        if let Some(cache) = world_gen.global_structure_cache() {
            cache.ensure_strongholds_generated();
        }

        let target_format =
            crate::chunk::io::converter::RegionFormat::from_config(&level_config.chunk);
        for folder in [&level_folder.region_folder, &level_folder.entities_folder] {
            match crate::chunk::io::converter::convert_folder(folder, &level_config.chunk, true) {
                Ok(stats) if stats.regions_converted > 0 => {
                    info!(
                        "Converted {} region(s) ({} chunk(s)) in {} to {} format",
                        stats.regions_converted,
                        stats.chunks_converted,
                        folder.display(),
                        target_format.name()
                    );
                }
                Ok(_) => {}
                Err(err) => {
                    error!(
                        "Failed converting region files in {} to {} format: {err}",
                        folder.display(),
                        target_format.name()
                    );
                }
            }
        }

        let chunk_saver = Arc::new(ChunkSaver::from_config(&level_config.chunk));
        let entity_saver = Arc::new(EntitySaver::from_config(&level_config.chunk));

        let (send_saves, receive_saves) = mpsc::unbounded_channel();
        let level_channel = Arc::new(LevelChannel::new());
        let thread_tracker = Mutex::new(Vec::new());
        let listener = Arc::new(ChunkListener::new());
        let rt_handle = OnceLock::new();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let _ = rt_handle.set(handle);
        }

        let level_ref = Arc::new(Self {
            seed,
            world_portal: ArcSwap::new(Arc::new(None)),
            world_gen: ArcSwap::new(world_gen),
            level_folder,
            lighting_config: level_config.lighting,
            light_engine: DynamicLightEngine::new(dim_min_y, dim_min_y + dim_height),
            chunk_saver,
            checkpoint_fence,
            entity_saver,
            schedule_tick_counts: AtomicU64::new(0),
            loaded_chunks: Arc::default(),
            loaded_chunk_changes: Arc::new(SegQueue::new()),
            loaded_entity_chunks: Arc::default(),
            entity_chunk_handoff: Mutex::default(),
            chunks_with_scheduled_ticks: Arc::new(dashmap::DashSet::new()),
            chunk_loading: Mutex::new(ChunkLoading::new(level_channel.clone())),
            chunk_watchers: Arc::new(DashMap::new()),
            tasks: TaskTracker::new(),
            rt_handle,
            chunk_system_tasks: TaskTracker::new(),
            cancel_token: CancellationToken::new(),
            shut_down_chunk_system: AtomicBool::new(false),
            should_save: AtomicBool::new(false),
            manual_saves: send_saves,
            should_unload: AtomicBool::new(false),
            save_enabled: AtomicBool::new(true),
            autosave_ticks: level_config.autosave_ticks,
            level_channel: level_channel.clone(),
            thread_tracker,
            chunk_listener: listener.clone(),
        });

        GenerationSchedule::create(
            4,
            level_ref.clone(),
            level_channel,
            listener,
            receive_saves,
            level_ref
                .thread_tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_mut(),
        );

        level_ref
    }

    pub fn set_world_gen(&self, generator: Arc<WorldGenerator>) {
        self.world_gen.store(generator);
    }

    #[must_use]
    pub fn world_gen(&self) -> Arc<WorldGenerator> {
        self.world_gen.load_full()
    }

    pub fn spawn_entity_generation(self: &Arc<Self>, pos: Vector2<i32>) {
        self.finish_entity_chunk_load(pos, Self::empty_entity_chunk(pos));
    }

    fn empty_entity_chunk(pos: Vector2<i32>) -> SyncEntityChunk {
        Arc::new(ChunkEntityData {
            x: pos.x,
            z: pos.y,
            data: Mutex::new(Vec::new()),
            live: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
        })
    }

    fn finish_entity_chunk_load(&self, pos: Vector2<i32>, chunk: SyncEntityChunk) {
        let mut pending = self
            .entity_chunk_handoff
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = self
            .loaded_entity_chunks
            .entry(pos)
            .or_insert(chunk)
            .clone();
        if let Some(waiters) = pending.remove(&pos) {
            for waiter in waiters {
                let _ = waiter.send(current.clone());
            }
        }
    }

    /// Spawns a task associated with this world. All tasks spawned with this method are awaited
    /// when the client. This means tasks should complete in a reasonable (no looping) amount of time.
    pub fn spawn_task<F>(&self, task: F) -> Option<JoinHandle<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        if let Some(handle) = self.rt_handle.get() {
            Some(self.tasks.spawn_on(task, handle))
        } else if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let _ = self.rt_handle.set(handle.clone());
            Some(self.tasks.spawn_on(task, &handle))
        } else {
            None
        }
    }

    pub async fn shutdown(&self) {
        self.shutdown_with_clock(None).await;
    }

    pub async fn shutdown_with_clock(
        &self,
        clock: Option<crate::world_info::clock::WorldClockSnapshot>,
    ) {
        let world_id = self.level_folder.root_folder.display();
        info!("Saving level ({})...", world_id);
        self.cancel_token.cancel();
        self.shut_down_chunk_system.store(true, Ordering::Relaxed);
        self.level_channel.notify();

        self.tasks.close();
        self.chunk_system_tasks.close();

        let handles = {
            let mut lock = self
                .thread_tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            lock.drain(..).collect::<Vec<_>>()
        };

        let handle_count = handles.len();
        info!("Joining {} threads for {}...", handle_count, world_id);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = std::thread::Builder::new()
            .name("Thread-Joiner".into())
            .spawn(move || {
                let mut failed_count = 0;
                for handle in handles {
                    if handle.join().is_err() {
                        failed_count += 1;
                    }
                }
                let _ = tx.send(failed_count);
            });

        match timeout(Duration::from_secs(3), rx).await {
            Ok(Ok(failed_count)) => {
                if failed_count > 0 {
                    warn!(
                        "{} threads failed to join properly for {}.",
                        failed_count, world_id
                    );
                }
            }
            Ok(Err(_)) => {
                warn!("Thread join task panicked for {}.", world_id);
            }
            Err(_) => {
                warn!("Timed out waiting for threads to join for {}.", world_id);
            }
        }

        self.tasks.wait().await;
        self.chunk_system_tasks.wait().await;

        // save all chunks currently in memory
        let chunks_to_write = self
            .loaded_entity_chunks
            .iter()
            .map(|chunk| (*chunk.key(), chunk.value().clone()))
            .collect::<Vec<_>>();
        self.loaded_entity_chunks.clear();

        self.write_entity_chunks(chunks_to_write).await;

        info!("Flushing chunk data to disk for {}...", world_id);
        if let Some(_access) = self.checkpoint_access().await
            && let Err(err) = self.chunk_saver.flush_all().await
        {
            error!("Failed to flush chunk data for {world_id}: {err}");
        }
        info!("Flushing entity data to disk for {}...", world_id);
        if let Err(err) = self.entity_saver.flush_all().await {
            error!("Failed to flush entity data for {world_id}: {err}");
        }

        // The scheduler and its writer have stopped, so an older pending save
        // cannot overwrite the last clock captured on the world owner.
        if let Some(clock) = clock
            && let Err(err) = self.persist_clock(clock).await
        {
            error!("{err}");
        }
    }

    pub(crate) async fn checkpoint_access(&self) -> Option<tokio::sync::OwnedRwLockReadGuard<()>> {
        self.checkpoint_fence.read(&self.cancel_token).await
    }

    pub(crate) async fn checkpoint_terrain_read(
        &self,
    ) -> Option<tokio::sync::OwnedRwLockReadGuard<()>> {
        self.checkpoint_fence.read_terrain(&self.cancel_token).await
    }

    /// Queues a save behind the scheduler's previous terrain writes and waits for the writer.
    pub async fn save_regions(
        &self,
        checkpoint_chunks: FxHashSet<Vector2<i32>>,
    ) -> Result<(), String> {
        self.queue_region_save(checkpoint_chunks, None).await
    }

    /// Queues a terrain save and its clock in the same writer order. Enqueue
    /// while the caller holds the clock, then await the receipt without it.
    pub fn queue_region_save(
        &self,
        checkpoint_chunks: FxHashSet<Vector2<i32>>,
        clock: Option<crate::world_info::clock::WorldClockSnapshot>,
    ) -> impl Future<Output = Result<(), String>> + use<'_> {
        let (send, receive) = oneshot::channel();
        let queued = self
            .manual_saves
            .send(SaveRequest {
                completion: Some(send),
                checkpoint_chunks,
                clock,
            })
            .map_err(|_| "Region scheduler stopped".to_string());
        self.should_save.store(true, Ordering::Release);
        self.level_channel.notify();

        async move {
            queued?;
            select! {
                biased;
                received = receive => received.map_err(|_| "Region writer stopped".to_string())?,
                () = self.checkpoint_fence.pending() => Err("A world checkpoint is awaiting recovery".to_string()),
                () = self.cancel_token.cancelled() => Err("Level is shutting down".to_string()),
            }
        }
    }

    pub fn queue_autosave(&self, clock: crate::world_info::clock::WorldClockSnapshot) {
        // Coalesce while the scheduler is busy, just as the periodic terrain save does.
        if !self.should_save.swap(true, Ordering::AcqRel) {
            let _ = self.manual_saves.send(SaveRequest {
                completion: None,
                checkpoint_chunks: FxHashSet::default(),
                clock: Some(clock),
            });
            // The scheduler may have taken the flag before the request arrived.
            self.should_save.store(true, Ordering::Release);
            self.level_channel.notify();
        }
    }

    pub(crate) async fn persist_clock(
        &self,
        clock: crate::world_info::clock::WorldClockSnapshot,
    ) -> Result<(), String> {
        let folder = self.level_folder.clone();
        crate::chunk::io::run_blocking(move || clock.write(&folder.dim_folder))
            .await
            .map_err(|err| format!("World clock writer stopped: {err}"))?
            .map_err(|err| format!("Failed to save world clock: {err}"))
    }

    pub(crate) async fn flush_regions(&self) -> Result<(), String> {
        let save_entities = {
            let _handoff = self
                .entity_chunk_handoff
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entities = self
                .loaded_entity_chunks
                .iter()
                .filter(|chunk| chunk.is_dirty())
                .map(|chunk| (*chunk.key(), chunk.value().clone()))
                .collect();
            self.entity_saver.queue_saves(&self.level_folder, entities)
        };
        let entity_save = save_entities.await;
        let (terrain_flush, entity_flush) =
            tokio::join!(self.chunk_saver.flush_all(), self.entity_saver.flush_all());
        entity_save
            .map_err(|err| format!("Failed to save entities: {err}"))
            .and(terrain_flush.map_err(|err| format!("Failed to flush terrain: {err}")))
            .and(entity_flush.map_err(|err| format!("Failed to flush entities: {err}")))
    }

    /// Writes region files whose saved chunks have waited long enough.
    pub async fn flush_due_regions(&self) {
        self.chunk_saver.flush_due().await;
        self.entity_saver.flush_due().await;
    }

    pub fn loaded_chunk_count(&self) -> usize {
        self.loaded_chunks.len()
    }

    pub fn list_cached(&self) {
        for entry in self.loaded_chunks.iter() {
            debug!("In map: {:?}", entry.key());
        }
    }

    /// Registers a player's newly watched chunks and returns the positions whose first
    /// watcher needs to request entity data.
    pub fn mark_chunks_as_newly_watched(&self, chunks: &[Vector2<i32>]) -> Vec<Vector2<i32>> {
        let mut first = Vec::new();
        for &pos in chunks {
            match self.chunk_watchers.entry(pos) {
                Entry::Occupied(mut entry) => {
                    *entry.get_mut() = entry.get().saturating_add(1);
                }
                Entry::Vacant(entry) => {
                    entry.insert(1);
                    first.push(pos);
                }
            }
        }
        first
    }

    /// Marks chunks no longer "watched" by a unique player. When no players are watching a chunk,
    /// it is removed from memory. Should only be called on chunks the player was watching before
    pub fn mark_chunks_as_not_watched(
        &self,
        chunks: impl IntoIterator<Item = impl std::borrow::Borrow<Vector2<i32>>>,
    ) -> Vec<Vector2<i32>> {
        let mut chunks_to_clean = Vec::new();

        for chunk in chunks {
            let chunk = chunk.borrow();
            if let Entry::Occupied(mut entry) = self.chunk_watchers.entry(*chunk) {
                *entry.get_mut() = entry.get().saturating_sub(1);
                if *entry.get() == 0 {
                    entry.remove();
                    chunks_to_clean.push(*chunk);
                }
            }
        }

        chunks_to_clean
    }

    /// Returns whether the chunk should be removed from memory
    #[inline]
    pub fn mark_chunk_as_not_watched(&self, chunk: Vector2<i32>) -> bool {
        !self.mark_chunks_as_not_watched([chunk]).is_empty()
    }

    pub fn clean_entity_chunks(
        self: &Arc<Self>,
        chunks: impl IntoIterator<Item = impl std::borrow::Borrow<Vector2<i32>>>,
    ) {
        // Keep the removal and disk-queue handoff together for a concurrent manual save.
        let _handoff = self
            .entity_chunk_handoff
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let chunks_to_process: Vec<_> = chunks
            .into_iter()
            .filter_map(|pos_borrow| {
                let pos = pos_borrow.borrow();
                // Use the same map order as clean_memory. Watchers cannot
                // attach until the cached entry has been removed.
                let Entry::Occupied(chunk) = self.loaded_entity_chunks.entry(*pos) else {
                    return None;
                };
                let Entry::Vacant(_watch_slot) = self.chunk_watchers.entry(*pos) else {
                    return None;
                };
                Some(chunk.remove_entry())
            })
            .collect();

        if chunks_to_process.is_empty() {
            return;
        }

        let save = self
            .entity_saver
            .queue_saves(&self.level_folder, chunks_to_process);
        self.spawn_task(async move {
            if let Err(err) = save.await {
                error!("Failed writing entity chunks to disk: {err}");
            }
        });
    }

    pub fn get_tick_data(
        &self,
        active_chunks: &FxHashSet<Vector2<i32>>,
        random_tick_speed: i64,
    ) -> TickData {
        self.get_tick_data_with_regions(active_chunks, random_tick_speed, |chunks| {
            vec![ScheduledTickGroup {
                chunks,
                step: ScheduledTickStep::Advance(None),
            }]
        })
    }

    /// Collects due ticks using the world's current ownership of scheduled chunks.
    /// The partition must return each supplied position once.
    pub fn get_tick_data_with_regions(
        &self,
        active_chunks: &FxHashSet<Vector2<i32>>,
        random_tick_speed: i64,
        partition: impl FnOnce(Vec<Vector2<i32>>) -> Vec<ScheduledTickGroup>,
    ) -> TickData {
        let samples_per_section = random_tick_speed.max(0);

        let mut ticks = TickData {
            block_ticks: Vec::new(),
            fluid_ticks: Vec::new(),
            random_ticks: Vec::with_capacity(active_chunks.len() * 3),
        };

        // 1. Process active chunks (random ticks, block entities)
        for pos in active_chunks {
            if let Some(chunk) = self.loaded_chunks.get(pos) {
                let chunk = chunk.value();
                let chunk_x_base = chunk.x * 16;
                let chunk_z_base = chunk.z * 16;
                let section_count = chunk.section.count;

                // Use the bitmask to skip sections
                let mask = chunk.section.randomly_ticking_mask.load(Ordering::Relaxed);
                if mask != 0 {
                    let sections = chunk
                        .section
                        .block_sections
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let min_y = chunk.section.min_y;

                    for i in 0..section_count {
                        if (mask & (1 << i)) == 0 {
                            continue;
                        }
                        let y_base = min_y + (i as i32 * 16);
                        for _ in 0..samples_per_section {
                            let r = rand::random::<u32>();
                            let x_offset = (r & 0xF) as usize;
                            let z_offset = (r >> 8 & 0xF) as usize;
                            let y_in_section = ((r >> 4) & 0xF) as usize;

                            let block_state_id = sections[i].get(x_offset, y_in_section, z_offset);
                            let tick_block = has_random_ticks(block_state_id);
                            let tick_fluid = has_random_ticking_fluid(block_state_id);
                            if tick_block || tick_fluid {
                                ticks.random_ticks.push(RandomTickSample {
                                    position: BlockPos::new(
                                        chunk_x_base + x_offset as i32,
                                        y_base + y_in_section as i32,
                                        chunk_z_base + z_offset as i32,
                                    ),
                                    tick_block,
                                    tick_fluid,
                                });
                            }
                        }
                    }
                }
            }
        }

        // Drop the index's shard guards before accessing the loaded chunks.
        let scheduled = self
            .chunks_with_scheduled_ticks
            .iter()
            .map(|p| *p)
            .collect();
        self.step_scheduled_chunk_groups(partition(scheduled), &mut ticks);

        // World callbacks can reach across regions and still use the global priority order.
        ticks.block_ticks.sort_unstable();
        ticks.fluid_ticks.sort_unstable();
        ticks
    }

    fn step_scheduled_chunk_groups(
        &self,
        mut groups: Vec<ScheduledTickGroup>,
        ticks: &mut TickData,
    ) {
        if groups.len() < 2 {
            if let Some(group) = groups.pop() {
                self.step_scheduled_chunks(group, ticks);
            }
        } else {
            let partials: Vec<_> = groups
                .into_par_iter()
                .map(|group| {
                    let mut data = TickData::default();
                    self.step_scheduled_chunks(group, &mut data);
                    data
                })
                .collect();
            for mut partial in partials {
                ticks.block_ticks.append(&mut partial.block_ticks);
                ticks.fluid_ticks.append(&mut partial.fluid_ticks);
            }
        }
    }

    fn step_scheduled_chunks(&self, group: ScheduledTickGroup, ticks: &mut TickData) {
        for pos in group.chunks {
            // Drain block and fluid work on the same side of a region timer step.
            if let Some(chunk) = self.loaded_chunks.get_mut(&pos) {
                let (mut blocks, mut fluids) = match group.step {
                    ScheduledTickStep::Advance(region_tick) => (
                        chunk.block_ticks.step_tick_at(region_tick),
                        chunk.fluid_ticks.step_tick_at(region_tick),
                    ),
                    ScheduledTickStep::Ready => (
                        chunk.block_ticks.take_ready(),
                        chunk.fluid_ticks.take_ready(),
                    ),
                };
                let pending = chunk.block_ticks.has_ticks() || chunk.fluid_ticks.has_ticks();
                if !blocks.is_empty()
                    || !fluids.is_empty()
                    || pending && matches!(group.step, ScheduledTickStep::Advance(_))
                {
                    chunk.mark_dirty(true);
                }
                ticks.block_ticks.append(&mut blocks);
                ticks.fluid_ticks.append(&mut fluids);

                if pending {
                    continue;
                }
            }
            self.chunks_with_scheduled_ticks.remove(&pos);
            // A new tick or replacement chunk may have been registered as we removed it.
            self.track_scheduled_chunk(&pos);
        }
    }

    /// Restores pending work when a loaded chunk becomes active.
    pub fn track_scheduled_chunk(&self, pos: &Vector2<i32>) {
        if let Some(chunk) = self.loaded_chunks.get(pos)
            && (chunk.block_ticks.has_ticks() || chunk.fluid_ticks.has_ticks())
        {
            self.chunks_with_scheduled_ticks.insert(*pos);
        }
    }

    pub fn clean_entity_chunk(self: &Arc<Self>, chunk: &Vector2<i32>) {
        self.clean_entity_chunks([*chunk]);
    }

    pub fn is_chunk_watched(&self, chunk: &Vector2<i32>) -> bool {
        self.chunk_watchers.get(chunk).is_some()
    }

    pub fn clean_memory(self: &Arc<Self>) -> Vec<Vector2<i32>> {
        self.chunk_watchers.retain(|_, watcher| *watcher != 0);

        let entity_chunks_to_remove: Vec<_> = self
            .loaded_entity_chunks
            .iter()
            .filter(|entry| !self.chunk_watchers.contains_key(entry.key()))
            .map(|entry| *entry.key())
            .collect();

        // We do not clean them here because we want the caller to save any active entities in them first.

        // if the difference is too big, we can shrink the loaded chunks
        // (1024 chunks is the equivalent to a 32x32 chunks area)
        if self.chunk_watchers.capacity() - self.chunk_watchers.len() >= 4096 {
            self.chunk_watchers.shrink_to_fit();
        }

        if self.loaded_chunks.capacity() - self.loaded_chunks.len() >= 4096 {
            self.loaded_chunks.shrink_to_fit();
        }

        if self.loaded_entity_chunks.capacity() - self.loaded_entity_chunks.len() >= 4096 {
            self.loaded_entity_chunks.shrink_to_fit();
        }
        entity_chunks_to_remove
    }

    pub async fn get_or_fetch_chunk<R, F: Fn(&SyncChunk) -> R>(
        self: &Arc<Self>,
        pos: Vector2<i32>,
        f: F,
    ) -> R {
        // Check if already in memory
        if let Some(res) = self.read_chunk_sync(&pos, &f) {
            return res;
        }
        let chunk = self.fetch_chunk(pos).await;
        if self.loaded_chunks.insert(pos, chunk.clone()).is_none() {
            self.loaded_chunk_changes
                .push(LoadedChunkChange::Loaded(pos));
        }
        f(&chunk)
    }

    /// Returns whether the chunk at `pos` is currently loaded in memory or exists on disk.
    pub async fn is_chunk_generated(&self, pos: Vector2<i32>) -> bool {
        if self.is_chunk_loaded(&pos) {
            return true;
        }
        self.chunk_saver
            .is_chunk_generated(&self.level_folder, pos)
            .await
    }

    /// Deletes an unloaded chunk (and its entity chunk) from disk storage.
    /// Returns `false` if the chunk is currently loaded or did not exist on disk.
    pub async fn delete_chunk(&self, pos: Vector2<i32>) -> bool {
        let access = select! {
            () = self.checkpoint_fence.pending() => None,
            access = self.checkpoint_access() => access,
        };
        let Some(_access) = access else {
            return false;
        };
        if self.is_chunk_loaded(&pos) {
            return false;
        }
        let chunk_deleted = self.chunk_saver.delete_chunk(&self.level_folder, pos).await;
        let entity_deleted = self
            .entity_saver
            .delete_chunk(&self.level_folder, pos)
            .await;
        chunk_deleted || entity_deleted
    }

    pub fn loaded_chunk_changes(&self) -> impl Iterator<Item = LoadedChunkChange> + '_ {
        std::iter::from_fn(|| self.loaded_chunk_changes.pop())
    }

    async fn fetch_chunk(self: &Arc<Self>, pos: Vector2<i32>) -> SyncChunk {
        let recv = self.chunk_listener.add_single_chunk_listener(pos);

        {
            let mut lock = self
                .chunk_loading
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            lock.add_force_ticket(pos);
            lock.send_change();
        };

        let chunk = recv
            .await
            .unwrap_or_else(|_| ChunkData::empty_sync(pos.x, pos.y));

        {
            let mut lock = self
                .chunk_loading
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            lock.remove_force_ticket(pos);
            lock.send_change();
        };

        chunk
    }

    /// Shares one disk request per position across entity chunk readers. The same
    /// handoff lock orders cache lookup with retiring a chunk into the save queue.
    fn request_entity_chunks(
        self: &Arc<Self>,
        chunks: impl IntoIterator<Item = Vector2<i32>>,
    ) -> Vec<Result<SyncEntityChunk, oneshot::Receiver<SyncEntityChunk>>> {
        let mut to_fetch = Vec::new();
        let requested = {
            let mut pending = self
                .entity_chunk_handoff
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            chunks
                .into_iter()
                .map(|pos| {
                    if let Some(chunk) = self.get_entity_chunk_sync(&pos) {
                        return Ok(chunk);
                    }
                    let (sender, receiver) = oneshot::channel();
                    if !self.cancel_token.is_cancelled() {
                        match pending.entry(pos) {
                            HashEntry::Occupied(mut pending) => pending.get_mut().push(sender),
                            HashEntry::Vacant(pending) => {
                                pending.insert(vec![sender]);
                                to_fetch.push(pos);
                            }
                        }
                    }
                    Err(receiver)
                })
                .collect()
        };
        if !to_fetch.is_empty() {
            let level = self.clone();
            let abandoned = to_fetch.clone();
            if self
                .spawn_task(async move { level.fetch_entity_chunks(to_fetch).await })
                .is_none()
            {
                self.cancel_entity_chunk_loads(abandoned);
            }
        }
        requested
    }

    async fn fetch_entity_chunks(&self, positions: Vec<Vector2<i32>>) {
        let mut unfinished: FxHashSet<_> = positions.iter().copied().collect();
        let fetch = async {
            let (tx, mut rx) = mpsc::channel(positions.len());
            self.entity_saver
                .fetch_chunks(&self.level_folder, &positions, tx)
                .await;
            while let Some(record) = rx.recv().await {
                let (pos, chunk) = match record {
                    LoadedData::Loaded(chunk) => (Vector2::new(chunk.x, chunk.z), chunk),
                    LoadedData::Missing(pos) | LoadedData::Error((pos, _)) => {
                        (pos, Self::empty_entity_chunk(pos))
                    }
                };
                if unfinished.remove(&pos) {
                    self.finish_entity_chunk_load(pos, chunk);
                }
            }
        };
        select! {
            () = self.cancel_token.cancelled() => {},
            () = fetch => {},
        }
        self.cancel_entity_chunk_loads(unfinished);
    }

    fn cancel_entity_chunk_loads(&self, positions: impl IntoIterator<Item = Vector2<i32>>) {
        let mut positions = positions.into_iter().peekable();
        if positions.peek().is_none() {
            return;
        }
        let mut pending = self
            .entity_chunk_handoff
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for pos in positions {
            pending.remove(&pos);
        }
    }

    pub fn receive_entity_chunks(
        self: &Arc<Self>,
        chunks: Vec<Vector2<i32>>,
    ) -> Receiver<(Weak<ChunkEntityData>, bool)> {
        let (sender, receiver) = mpsc::channel(64);
        let level = self.clone();
        self.spawn_task(async move {
            let fetch = async {
                let mut loading = FuturesUnordered::new();
                for request in level.request_entity_chunks(chunks) {
                    match request {
                        Ok(chunk) => {
                            if sender.send((Arc::downgrade(&chunk), false)).await.is_err() {
                                return;
                            }
                        }
                        Err(receive) => loading.push(receive),
                    }
                }
                while let Some(result) = loading.next().await {
                    if let Ok(chunk) = result
                        && sender.send((Arc::downgrade(&chunk), true)).await.is_err()
                    {
                        return;
                    }
                }
            };

            select! {
                () = level.cancel_token.cancelled() => {},
                () = fetch => {},
            }
        });
        receiver
    }

    pub async fn get_entity_chunk(self: &Arc<Self>, pos: Vector2<i32>) -> SyncEntityChunk {
        match self.request_entity_chunks([pos]).pop() {
            Some(Ok(chunk)) => chunk,
            Some(Err(receive)) => receive
                .await
                .unwrap_or_else(|_| Self::empty_entity_chunk(pos)),
            None => Self::empty_entity_chunk(pos),
        }
    }

    pub fn get_block_state(&self, position: &BlockPos) -> BlockStateId {
        let (chunk_coordinate, relative) = position.chunk_and_chunk_relative_position();
        let id = self
            .read_chunk_sync(&chunk_coordinate, |chunk| {
                chunk.section.get_block_absolute_y(
                    relative.x as usize,
                    relative.y,
                    relative.z as usize,
                )
            })
            .flatten();

        id.unwrap_or(Block::VOID_AIR.default_state.id)
    }

    pub fn set_block_state(
        &self,
        position: &BlockPos,
        block_state_id: BlockStateId,
    ) -> BlockStateId {
        let (chunk_coordinate, relative) = position.chunk_and_chunk_relative_position();
        self.read_chunk_sync(&chunk_coordinate, |chunk| {
            let replaced_block_state_id = chunk.set_block_absolute_y(
                relative.x as usize,
                relative.y,
                relative.z as usize,
                block_state_id,
            );
            if replaced_block_state_id != block_state_id {
                chunk.mark_dirty(true);
            }
            replaced_block_state_id
        })
        .unwrap_or(Block::VOID_AIR.default_state.id)
    }

    pub async fn write_chunks(&self, chunks_to_write: Vec<(Vector2<i32>, SyncChunk)>) {
        if chunks_to_write.is_empty() {
            return;
        }
        let Some(_access) = self.checkpoint_access().await else {
            return;
        };

        let chunk_saver = self.chunk_saver.clone();
        let level_folder = self.level_folder.clone();

        trace!("Sending chunks to ChunkIO {:}", chunks_to_write.len());
        if let Err(error) = chunk_saver
            .save_chunks(&level_folder, chunks_to_write)
            .await
        {
            error!("Failed writing Chunk to disk {error}");
        }
    }

    pub async fn write_entity_chunks(&self, chunks_to_write: Vec<(Vector2<i32>, SyncEntityChunk)>) {
        if chunks_to_write.is_empty() {
            return;
        }

        let chunk_saver = self.entity_saver.clone();
        let level_folder = self.level_folder.clone();

        trace!("Sending chunks to ChunkIO {:}", chunks_to_write.len());
        if let Err(error) = chunk_saver
            .save_chunks(&level_folder, chunks_to_write)
            .await
        {
            error!("Failed writing Chunk to disk {error}");
        }
    }

    pub fn is_chunk_loaded(&self, coordinates: &Vector2<i32>) -> bool {
        self.loaded_chunks.contains_key(coordinates)
    }

    pub fn read_chunk_sync<R, F: Fn(&SyncChunk) -> R>(
        &self,
        coordinates: &Vector2<i32>,
        f: F,
    ) -> Option<R> {
        self.loaded_chunks.get(coordinates).map(|x| f(x.value()))
    }

    /// Reads a block without waiting for a loaded chunk or palette lock.
    pub fn try_read_block_state(&self, position: BlockPos) -> Option<BlockStateId> {
        self.try_with_block_state(position, |_, state| state)
    }

    /// Pins the loaded chunk and its block palette while using the state. The callback
    /// must not acquire either lock again or wait for work using them.
    pub fn try_with_block_state<R>(
        &self,
        position: BlockPos,
        read: impl FnOnce(&ChunkData, BlockStateId) -> R,
    ) -> Option<R> {
        let (pos, relative) = position.chunk_and_chunk_relative_position();
        let TryResult::Present(chunk) = self.loaded_chunks.try_get(&pos) else {
            return None;
        };
        let sections = &chunk.section;
        let palettes = sections.block_sections.try_read()?;
        let state = sections.read_block_from_palettes(&palettes, relative)?;
        Some(read(chunk.value(), state))
    }

    /// Runs a local update with an all-air path and its destination biome pinned.
    /// The callback must not acquire the chunk or either palette lock again.
    pub fn try_with_clear_chunk_path<R>(
        &self,
        min: BlockPos,
        max: BlockPos,
        destination: BlockPos,
        apply: impl FnOnce(&'static Biome) -> R,
    ) -> Option<R> {
        let chunk_pos = destination.chunk_position();
        if min.chunk_position() != chunk_pos || max.chunk_position() != chunk_pos {
            return None;
        }
        self.try_with_clear_regional_path(min, max, destination, |_| true, apply)
    }

    /// Pins an all-air sweep across at most four chunks and the destination biome.
    /// The region permit must cover every chunk accepted by `owns` until this returns.
    /// Neither callback can wait for work that uses the pinned chunks or palettes.
    pub fn try_with_clear_regional_path<R>(
        &self,
        min: BlockPos,
        max: BlockPos,
        destination: BlockPos,
        owns: impl Fn(Vector2<i32>) -> bool,
        apply: impl FnOnce(&'static Biome) -> R,
    ) -> Option<R> {
        self.try_with_regional_path(
            min,
            max,
            owns,
            |read| {
                for position in BlockPos::iterate(min, max) {
                    if !read(position)?.to_state().is_air() {
                        return None;
                    }
                }
                Some((destination, ()))
            },
            |(), biome| apply(biome),
        )
    }

    /// Pins loaded block palettes, prepares movement, and reads the actual destination biome.
    /// The region permit must cover every chunk accepted by `owns` until this returns.
    /// Callbacks must not wait for work that uses the pinned chunks or palettes.
    pub fn try_with_regional_path<T, R>(
        &self,
        min: BlockPos,
        max: BlockPos,
        owns: impl Fn(Vector2<i32>) -> bool,
        prepare: impl FnOnce(&dyn Fn(BlockPos) -> Option<BlockStateId>) -> Option<(BlockPos, T)>,
        apply: impl FnOnce(T, &'static Biome) -> R,
    ) -> Option<R> {
        let first = min.chunk_position();
        let last = max.chunk_position();
        if last.x - first.x > 1 || last.y - first.y > 1 {
            return None;
        }

        let positions =
            (first.x..=last.x).flat_map(|x| (first.y..=last.y).map(move |z| Vector2::new(x, z)));
        let mut chunks = [None, None, None, None];
        for (slot, pos) in chunks.iter_mut().zip(positions) {
            if !owns(pos) {
                return None;
            }
            let TryResult::Present(chunk) = self.loaded_chunks.try_get(&pos) else {
                return None;
            };
            *slot = Some(chunk);
        }

        let mut palettes = [None, None, None, None];
        for (slot, chunk) in palettes.iter_mut().zip(chunks.iter().flatten()) {
            let sections = &chunk.section;
            *slot = Some((*chunk.key(), sections, sections.block_sections.try_read()?));
        }
        let palette_for = |pos| palettes.iter().flatten().find(|(chunk, ..)| *chunk == pos);
        let read = |position: BlockPos| {
            let (pos, local) = position.chunk_and_chunk_relative_position();
            let (_, sections, blocks) = palette_for(pos)?;
            sections.read_block_from_palettes(blocks, local)
        };

        let (destination, prepared) = prepare(&read)?;
        let (chunk, local) = destination.chunk_and_chunk_relative_position();
        let (_, sections, _blocks) = palette_for(chunk)?;
        let biomes = sections.biome_sections.try_read()?;
        let id = sections.read_rough_biome_from_palettes(
            &biomes,
            local.x as usize,
            local.y,
            local.z as usize,
        );
        let biome = Biome::from_id(id.unwrap_or(0)).unwrap_or(&Biome::THE_VOID);
        palettes
            .iter()
            .flatten()
            .all(|(pos, ..)| owns(*pos))
            .then(|| apply(prepared, biome))
    }

    pub fn read_entity_chunk_sync<R, F: Fn(&SyncEntityChunk) -> R>(
        &self,
        coordinates: &Vector2<i32>,
        f: F,
    ) -> Option<R> {
        self.loaded_entity_chunks
            .get(coordinates)
            .map(|x| f(x.value()))
    }

    pub fn get_rough_biome(&self, position: &BlockPos) -> &'static Biome {
        let (chunk_coordinate, relative) = position.chunk_and_chunk_relative_position();
        let id = self.read_chunk_sync(&chunk_coordinate, |chunk| {
            chunk.section.get_rough_biome_absolute_y(
                relative.x as usize,
                relative.y,
                relative.z as usize,
            )
        });
        Biome::from_id(id.flatten().unwrap_or(0)).unwrap_or(&Biome::THE_VOID)
    }

    pub fn get_entity_chunk_sync(&self, pos: &Vector2<i32>) -> Option<SyncEntityChunk> {
        self.loaded_entity_chunks
            .get(pos)
            .map(|x| x.value().clone())
    }

    #[must_use]
    pub fn live_entity_chunk_positions(&self) -> Vec<Vector2<i32>> {
        self.loaded_entity_chunks
            .iter()
            .filter(|entry| entry.value().live.load(Ordering::Relaxed))
            .map(|entry| *entry.key())
            .collect()
    }

    pub async fn get_or_fetch_entity_chunk<R, F: Fn(&SyncEntityChunk) -> R>(
        self: &Arc<Self>,
        pos: Vector2<i32>,
        f: F,
    ) -> R {
        if let Some(res) = self.read_entity_chunk_sync(&pos, &f) {
            return res;
        }
        let chunk = self.get_entity_chunk(pos).await;
        f(&chunk)
    }

    pub fn try_get_entity_chunk(
        &self,
        coordinates: Vector2<i32>,
    ) -> Option<dashmap::mapref::one::Ref<'_, Vector2<i32>, Arc<ChunkEntityData>>> {
        self.loaded_entity_chunks.try_get(&coordinates).try_unwrap()
    }

    /// Pins the current entity chunk and a watcher without waiting for either map.
    /// The callback must not acquire these maps again.
    pub fn try_with_watched_entity_chunk<R>(
        &self,
        coordinates: Vector2<i32>,
        apply: impl FnOnce(&SyncEntityChunk) -> R,
    ) -> Option<R> {
        let watchers = self.chunk_watchers.try_get(&coordinates).try_unwrap()?;
        if *watchers == 0 {
            return None;
        }
        let chunk = self.try_get_entity_chunk(coordinates)?;
        Some(apply(chunk.value()))
    }

    /// Commits against the same cached entity chunk while its watcher slot is vacant.
    /// The callback must not access either map or perform disk I/O.
    pub fn try_with_unwatched_entity_chunk(
        &self,
        coordinates: Vector2<i32>,
        expected: &SyncEntityChunk,
        apply: impl FnOnce(),
    ) -> bool {
        let Some(Entry::Vacant(_watch_slot)) = self.chunk_watchers.try_entry(coordinates) else {
            return false;
        };
        let Some(chunk) = self.try_get_entity_chunk(coordinates) else {
            return false;
        };
        if !Arc::ptr_eq(chunk.value(), expected) {
            return false;
        }
        apply();
        true
    }

    /// Reserves tick orders for `entities` entities; entity `i` starts at
    /// `first + i * ORDERS_PER_ENTITY`.
    pub fn reserve_entity_tick_orders(&self, entities: usize) -> u64 {
        self.schedule_tick_counts.fetch_add(
            entities as u64 * order::ORDERS_PER_ENTITY,
            Ordering::Relaxed,
        )
    }

    fn next_scheduled_tick_order(&self) -> u64 {
        order::next_entity_order()
            .unwrap_or_else(|| self.schedule_tick_counts.fetch_add(1, Ordering::Relaxed))
    }

    /// Conditionally writes one block within the caller's current region extent.
    /// `None` defers the edit, while `Some(false)` reports an unloaded or changed target.
    pub fn try_apply_regional_block_edit(
        &self,
        expected_chunk: &SyncChunk,
        position: BlockPos,
        expected: BlockStateId,
        replacement: BlockStateId,
        owns: impl Fn(Vector2<i32>) -> bool,
        validate: impl Fn(&dyn Fn(BlockPos) -> Option<BlockStateId>) -> bool,
    ) -> Option<bool> {
        let pos = position.chunk_position();
        if !owns(pos) {
            return None;
        }
        match self.loaded_chunks.try_get(&pos) {
            TryResult::Present(chunk) if Arc::ptr_eq(chunk.value(), expected_chunk) => {}
            TryResult::Present(_) | TryResult::Absent => return Some(false),
            TryResult::Locked => return None,
        }
        apply_regional_chunk_tick(
            &self.loaded_chunks,
            expected_chunk,
            owns,
            |chunk, read, commit| {
                chunk.try_apply_block_edit(position, expected, replacement, read, commit, &validate)
            },
        )
    }

    /// Commits scheduled water movement and its follow-up timers in a loaded chunk.
    ///
    /// `owns` must be backed by the same region permit as for block properties.
    pub fn apply_regional_water_ticks(
        &self,
        expected: &SyncChunk,
        limit: usize,
        owns: impl Fn(Vector2<i32>) -> bool,
        evaluate: impl Fn(
            BlockPos,
            BlockStateId,
            &dyn Fn(BlockPos) -> Option<BlockStateId>,
        ) -> Option<DownwardWaterTick>,
    ) -> Option<Vec<(BlockPos, BlockStateId, BlockStateId)>> {
        apply_regional_chunk_tick(
            &self.loaded_chunks,
            expected,
            owns,
            |chunk, read, can_commit| {
                chunk.try_apply_downward_water_ticks(limit, read, can_commit, &evaluate, || {
                    self.next_scheduled_tick_order()
                })
            },
        )
    }

    pub fn schedule_block_tick(
        &self,
        block: &Block,
        block_pos: BlockPos,
        delay: u8,
        priority: TickPriority,
    ) {
        let tick_order = self.next_scheduled_tick_order();
        let scheduled_tick = ScheduledTick {
            delay,
            position: block_pos,
            priority,
            // SAFETY: `block` is a valid reference that outlives this function call for scheduling.
            value: unsafe { &*std::ptr::from_ref::<Block>(block) },
        };

        let chunk_pos = block_pos.chunk_position();
        if self
            .read_chunk_sync(&chunk_pos, |chunk| {
                chunk.block_ticks.schedule_tick(&scheduled_tick, tick_order);
                chunk.mark_dirty(true);
            })
            .is_some()
        {
            self.chunks_with_scheduled_ticks.insert(chunk_pos);
        }
    }

    pub fn schedule_fluid_tick(
        &self,
        fluid: &Fluid,
        block_pos: BlockPos,
        delay: u8,
        priority: TickPriority,
    ) {
        let tick_order = self.next_scheduled_tick_order();
        let scheduled_tick = ScheduledTick {
            delay,
            position: block_pos,
            priority,
            // SAFETY: `fluid` is a valid reference that outlives this function call for scheduling.
            value: unsafe { &*std::ptr::from_ref::<Fluid>(fluid) },
        };

        let chunk_pos = block_pos.chunk_position();
        if self
            .read_chunk_sync(&chunk_pos, |chunk| {
                chunk.fluid_ticks.schedule_tick(&scheduled_tick, tick_order);
                chunk.mark_dirty(true);
            })
            .is_some()
        {
            self.chunks_with_scheduled_ticks.insert(chunk_pos);
        }
    }

    pub fn is_block_tick_scheduled(&self, block_pos: &BlockPos, block: &Block) -> bool {
        self.read_chunk_sync(&block_pos.chunk_position(), |chunk| {
            chunk.block_ticks.is_scheduled(*block_pos, block)
        })
        .unwrap_or(false)
    }

    pub fn is_fluid_tick_scheduled(&self, block_pos: &BlockPos, fluid: &Fluid) -> bool {
        self.read_chunk_sync(&block_pos.chunk_position(), |chunk| {
            chunk.fluid_ticks.is_scheduled(*block_pos, fluid)
        })
        .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_config::world::LevelConfig;
    use pumpkin_data::block_properties::RedstoneOreLikeProperties;
    use std::cell::Cell;
    use tempfile::TempDir;

    #[tokio::test]
    async fn diagonal_regional_path_requires_all_four_chunks() {
        let folder = TempDir::new().unwrap();
        let level = Level::from_root_folder(
            &LevelConfig::default(),
            folder.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        );
        let min = BlockPos::new(-1, 64, -1);
        let destination = BlockPos::new(0, 64, 0);
        // The item crosses diagonally, but its swept bounds also touch this corner.
        let corner = ChunkData::empty_sync(0, -1);
        let corner_pos = Vector2::new(0, -1);
        for (x, z) in [(-1, -1), (-1, 0), (0, 0)] {
            level
                .loaded_chunks
                .insert(Vector2::new(x, z), ChunkData::empty_sync(x, z));
        }
        let read = |owns_corner| {
            level.try_with_clear_regional_path(
                min,
                destination,
                destination,
                |pos| owns_corner || pos != corner_pos,
                |biome| biome.id,
            )
        };
        assert_eq!(read(true), None);
        level.loaded_chunks.insert(corner_pos, corner.clone());
        assert_eq!(read(false), None);

        corner.set_block_absolute_y(0, 64, 15, Block::STONE.default_state.id);
        assert_eq!(read(true), None);
        corner.set_block_absolute_y(0, 64, 15, Block::AIR.default_state.id);
        let busy = corner.section.block_sections.write().unwrap();
        assert_eq!(read(true), None);
        drop(busy);

        assert_eq!(read(true), Some(level.get_rough_biome(&destination).id));
        level.shutdown().await;
    }

    #[tokio::test]
    async fn regional_path_uses_the_resolved_destination_and_rechecks_ownership() {
        let folder = TempDir::new().unwrap();
        let level = Level::from_root_folder(
            &LevelConfig::default(),
            folder.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        );
        let chunk = ChunkData::empty_sync(0, 0);
        let upper = BlockPos::new(2, 64, 2);
        let lower = BlockPos::new(2, 63, 2);
        let floor = BlockPos::new(2, 62, 2);
        chunk.set_block_absolute_y(2, 62, 2, Block::STONE.default_state.id);
        for (pos, biome) in [(upper, &Biome::PLAINS), (lower, &Biome::DESERT)] {
            let index = ((pos.0.y - chunk.section.min_y) >> 2) as usize;
            chunk.section.set_relative_biome(0, index, 0, biome.id);
        }
        level.loaded_chunks.insert(upper.chunk_position(), chunk);

        let owns = Cell::new(true);
        let run = |retain| {
            owns.set(true);
            level.try_with_regional_path(
                floor,
                upper,
                |_| owns.get(),
                |read| {
                    assert_eq!(read(floor), Some(Block::STONE.default_state.id));
                    assert_eq!(read(BlockPos::new(16, 63, 2)), None);
                    owns.set(retain);
                    Some((lower, floor))
                },
                |support, biome| (support, biome.id),
            )
        };
        assert_eq!(run(true), Some((floor, Biome::DESERT.id)));
        assert_eq!(run(false), None);
        level.shutdown().await;
    }

    #[test]
    fn regional_batch_requires_a_current_owned_neighbor_snapshot() {
        let loaded = DashMap::with_hasher(FxBuildHasher);
        let source = ChunkData::empty_sync(0, 0);
        let neighbor = ChunkData::empty_sync(1, 0);
        let at = BlockPos::new(15, 64, 3);
        let across = BlockPos::new(16, 64, 3);
        let lamp = &Block::REDSTONE_LAMP;
        let lit = RedstoneOreLikeProperties { lit: true }.to_state_id(lamp);
        let dark = RedstoneOreLikeProperties { lit: false }.to_state_id(lamp);
        source.set_block_absolute_y(15, 64, 3, lit);
        source.block_ticks.schedule_tick(
            &ScheduledTick {
                position: at,
                value: lamp,
                delay: 0,
                priority: TickPriority::Normal,
            },
            0,
        );
        source.block_ticks.advance();
        neighbor.set_block_absolute_y(0, 64, 3, Block::STONE.default_state.id);
        loaded.insert(at.chunk_position(), source.clone());
        loaded.insert(across.chunk_position(), neighbor.clone());

        let plan = |owns_neighbor, invalidate: bool| {
            let owned = Cell::new(true);
            apply_regional_block_properties(
                &loaded,
                &source,
                4,
                |pos| owned.get() && (pos == at.chunk_position() || owns_neighbor),
                |_, _, _, read| {
                    if read(across)? != Block::STONE.default_state.id {
                        return None;
                    }
                    owned.set(!invalidate);
                    Some(dark)
                },
            )
        };
        let encodes = Cell::new(0);
        let encode = |_: &[crate::chunk::palette::BlockPalette]| {
            encodes.set(encodes.get() + 1);
            Ok(Vec::new())
        };
        source.section.block_sections.bedrock_blobs(encode).unwrap();
        assert_eq!(plan(false, false), None);
        let busy_palette = neighbor.section.block_sections.write().unwrap();
        assert_eq!(plan(true, false), None);
        drop(busy_palette);
        assert_eq!(plan(true, true), None);
        assert_eq!(source.section.get_block_absolute_y(15, 64, 3), Some(lit));
        assert!(source.block_ticks.is_scheduled(at, lamp));
        source.section.block_sections.bedrock_blobs(encode).unwrap();
        assert_eq!(encodes.get(), 1);

        assert_eq!(plan(true, false), Some(vec![(at, lit, dark)]));
        assert!(!source.block_ticks.is_scheduled(at, lamp));
        source.section.block_sections.bedrock_blobs(encode).unwrap();
        assert_eq!(encodes.get(), 2);
    }

    #[tokio::test]
    async fn entity_chunk_views_keep_every_cached_result() {
        let folder = TempDir::new().unwrap();
        let level = Level::from_root_folder(
            &LevelConfig::default(),
            folder.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        );
        let positions: Vec<_> = (0..96).map(|x| Vector2::new(x, 0)).collect();
        for &pos in &positions {
            level.spawn_entity_generation(pos);
        }
        let mut received = level.receive_entity_chunks(positions.clone());
        let mut delivered = Vec::new();
        while let Some((chunk, loaded)) = received.recv().await {
            assert!(!loaded);
            let chunk = chunk.upgrade().unwrap();
            delivered.push(Vector2::new(chunk.x, chunk.z));
        }
        level.shutdown().await;
        assert_eq!(delivered, positions);
    }

    #[tokio::test]
    async fn overlapping_entity_views_publish_one_cache_entry() {
        let folder = TempDir::new().unwrap();
        let level = Level::from_root_folder(
            &LevelConfig::default(),
            folder.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        );
        let pos = Vector2::new(1, 2);
        let before = Level::empty_entity_chunk(pos);
        before
            .data
            .lock()
            .unwrap()
            .push(pumpkin_nbt::compound::NbtCompound::new());
        before.mark_dirty(true);
        level.write_entity_chunks(vec![(pos, before)]).await;
        let (one, two) = tokio::join!(level.get_entity_chunk(pos), level.get_entity_chunk(pos));
        let entry = level.get_entity_chunk_sync(&pos).unwrap();
        assert!(Arc::ptr_eq(&one, &two) && Arc::ptr_eq(&one, &entry));
        assert_eq!(entry.data.lock().unwrap().len(), 1);
        level.shutdown().await;
    }

    #[tokio::test]
    async fn dimension_paths_26_2() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().to_path_buf();
        let config = LevelConfig::default();

        let overworld_level =
            Level::from_root_folder(&config, root.clone(), 0, Dimension::OVERWORLD);
        assert_eq!(
            overworld_level.level_folder.dim_folder,
            root.join("dimensions").join("minecraft").join("overworld")
        );
        assert_eq!(
            overworld_level.level_folder.region_folder,
            root.join("dimensions")
                .join("minecraft")
                .join("overworld")
                .join("region")
        );

        let nether_level = Level::from_root_folder(&config, root.clone(), 0, Dimension::THE_NETHER);
        assert_eq!(
            nether_level.level_folder.dim_folder,
            root.join("dimensions").join("minecraft").join("the_nether")
        );

        let end_level = Level::from_root_folder(&config, root.clone(), 0, Dimension::THE_END);
        assert_eq!(
            end_level.level_folder.dim_folder,
            root.join("dimensions").join("minecraft").join("the_end")
        );
    }

    #[tokio::test]
    async fn legacy_dimension_fallback() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().to_path_buf();
        let config = LevelConfig::default();

        // Create legacy directories
        std::fs::create_dir_all(root.join("region")).unwrap();
        std::fs::create_dir_all(root.join("DIM-1").join("region")).unwrap();
        std::fs::create_dir_all(root.join("DIM1").join("region")).unwrap();

        let overworld_level =
            Level::from_root_folder(&config, root.clone(), 0, Dimension::OVERWORLD);
        assert_eq!(overworld_level.level_folder.dim_folder, root);

        let nether_level = Level::from_root_folder(&config, root.clone(), 0, Dimension::THE_NETHER);
        assert_eq!(nether_level.level_folder.dim_folder, root.join("DIM-1"));

        let end_level = Level::from_root_folder(&config, root.clone(), 0, Dimension::THE_END);
        assert_eq!(end_level.level_folder.dim_folder, root.join("DIM1"));
    }
}
