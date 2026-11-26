use crate::block::registry::BlockRegistry;
use crate::command::commands::default_dispatcher;
use crate::command::commands::defaultgamemode::DefaultGamemode;
use crate::data::VanillaData;
use crate::data::player_server::{SAVED_WORLD_ROOT, ServerPlayerData};
use crate::entity::NBTStorage;
use crate::entity::player::world_transfer::{WorldTransfer, WorldTransferReservation};
use crate::item::registry::ItemRegistry;
use crate::net::authentication::fetch_mojang_public_keys;
use crate::net::{ClientPlatform, DisconnectReason, EncryptionError, GameProfile, PlayerConfig};
use crate::plugin::PluginManager;
use crate::plugin::loader::PluginLoader;
use crate::plugin::player::player_login::PlayerLoginEvent;
use crate::plugin::server::server_broadcast::ServerBroadcastEvent;
use crate::server::tick_rate_manager::ServerTickRateManager;
use crate::world::WorldPortal;
use crate::world::custom_bossbar::CustomBossbars;
use crate::{
    command::node::dispatcher::CommandDispatcher, entity::player::Player, world::World,
    world::map::MapManager,
};
use arc_swap::ArcSwap;
use connection_cache::{CachedBranding, CachedStatus};
use crossbeam::queue::SegQueue;
use key_store::KeyStore;
use pumpkin_data::dimension::Dimension;
use pumpkin_world::generation::generator::GeneratorInit;
use pumpkin_world::level::{Level, LevelFolder};
use pumpkin_world::world::WorldPortalExt;
use sculith_config::world::WorldPreset;
use sculith_config::{AdvancedConfiguration, BasicConfiguration, TelemetryConfig};
use sculith_util::permission::PermissionManager;
use sculith_util::text::color::NamedColor;
use tracing::{debug, error, info, warn};

use pumpkin_world::data::checkpoint::CheckpointFence;
use pumpkin_world::world_info::anvil::{
    AnvilLevelInfo, LEVEL_DAT_BACKUP_FILE_NAME, LEVEL_DAT_FILE_NAME,
};
use pumpkin_world::world_info::clock::WorldClockSnapshot;
use pumpkin_world::world_info::data_files::read_world_clocks;
use pumpkin_world::world_info::{
    GeneratorSettings, LevelData, WorldInfoError, WorldInfoReader, WorldInfoWriter,
};
use rand::seq::IndexedRandom;
use rayon::prelude::*;
use rsa::PublicKey as RsaPublicKey;
use sculith_protocol::java::client::login::CEncryptionRequest;
use sculith_protocol::java::client::play::{CChangeDifficulty, CTabList};
use sculith_protocol::{ClientPacket, java::client::config::CPluginMessage};
use sculith_util::Difficulty;
use sculith_util::text::TextComponent;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32};
use std::{fs, io};
use std::{future::Future, sync::atomic::Ordering, time::Duration};
use tokio::sync::OnceCell;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

mod block_edits;
pub mod builtin_game_tests;
mod connection_cache;
pub(crate) mod debug_profiler;
pub mod enchantment;
mod key_store;
pub(crate) mod player_commands;
mod player_inventory_edits;
mod player_session;
pub mod recipe;
mod region_scheduler;
pub mod scheduler;
pub mod seasonal_events;
pub mod server_test_manager;
mod storage_checkpoint;
pub mod tick_rate_manager;
pub mod ticker;
mod transfer_checkpoint;
mod world_membership;
mod world_metadata;
mod world_openings;
mod world_scheduler;

struct SavedPlayer {
    data: pumpkin_nbt::compound::NbtCompound,
    restore_position: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorldStep {
    NetworkOnly(Option<TickDeadline>),
    Coordinated,
    RegionalDeadlines(TickDeadline),
}

pub(crate) use block_edits::BlockEdits;
pub use block_edits::{BlockEditReceipt, BlockEditStatus};
use player_commands::PlayerCommands;
pub(crate) use player_inventory_edits::PlayerInventoryEdits;
pub use player_inventory_edits::{PlayerInventoryEditReceipt, PlayerInventoryEditStatus};
pub use recipe::RecipeManager;
pub(crate) use region_scheduler::TickDeadline;
pub(crate) use transfer_checkpoint::StorageTransferError;
use world_membership::WorldOperation;

use crate::data::advancement_data::AdvancementManager;

struct SuspendedPlayerFlushes(Vec<Arc<Player>>);

impl SuspendedPlayerFlushes {
    fn for_world(world: &World) -> Self {
        let mut suspended = Vec::new();
        for player in world.players.load().iter() {
            if let Some(client) = player.client.java() {
                client.suspend_flushing();
                suspended.push(player.clone());
            }
        }
        Self(suspended)
    }
}

impl Drop for SuspendedPlayerFlushes {
    fn drop(&mut self) {
        // Resume the same clients even if a connection was removed in this tick.
        for player in &self.0 {
            if let Some(client) = player.client.java() {
                client.resume_flushing();
            }
        }
    }
}

/// Represents a Minecraft server instance.
#[repr(C)]
pub struct Server {
    pub basic_config: BasicConfiguration,
    pub advanced_config: AdvancedConfiguration,
    pub shared_resources: Option<Arc<crate::resource_pack::SharedResources>>,
    pub telemetry_config: TelemetryConfig,
    /// Tracks active and completed player session durations for anonymous telemetry.
    pub telemetry_sessions: crate::telemetry::TelemetrySessionTracker,

    pub data: VanillaData,

    /// Plugin manager
    pub plugin_manager: Arc<PluginManager>,

    /// Permission manager for the server.
    pub permission_manager: Arc<PermissionManager>,

    /// Handles cryptographic keys for secure communication.
    key_store: OnceCell<Arc<KeyStore>>,
    /// Bedrock OIDC provider keys, fetched on startup for 1.26.10+ token validation.
    pub bedrock_oidc_keys: Arc<OnceCell<(String, pumpkin_auth::jwt::Jwks)>>,
    /// Cached Bedrock server private key (process-lifetime). Generated on first Bedrock login and reused.
    /// Manages server status information.
    listing: std::sync::Mutex<CachedStatus>,
    /// Saves server branding information.
    branding: CachedBranding,
    /// Saves and dispatches commands to appropriate handlers.
    pub command_dispatcher: ArcSwap<CommandDispatcher>,
    /// Block behaviour.
    pub block_registry: Arc<BlockRegistry>,
    /// Item behaviour.
    pub item_registry: Arc<ItemRegistry>,
    /// Manages multiple worlds within the server.
    pub worlds: ArcSwap<Vec<Arc<World>>>,
    /// All the dimensions that exist on the server.
    pub dimensions: Vec<Dimension>,
    /// Assigns unique IDs to containers.
    container_id: AtomicU32,
    pub recipe_manager: Arc<recipe::RecipeManager>,
    pub datapack_manager: Arc<crate::data::datapack::DatapackManager>,
    pub enchantment_manager: Arc<enchantment::EnchantmentManager>,
    /// Assigns unique IDs to maps.
    map_id: AtomicI32,
    /// Mojang's public keys, used for chat session signing
    /// Pulled from Mojang API on startup
    pub mojang_public_keys: ArcSwap<Vec<RsaPublicKey>>,
    /// The server's custom bossbars
    pub bossbars: std::sync::Mutex<CustomBossbars>,
    /// Manages all maps on the server
    pub map_manager: MapManager,
    /// The default gamemode when a player joins the server (reset every restart)
    pub defaultgamemode: std::sync::Mutex<DefaultGamemode>,
    /// Manages player data storage
    pub player_data_storage: ServerPlayerData,
    /// Command storage for `/data` command
    pub command_storage:
        std::sync::Mutex<std::collections::HashMap<String, pumpkin_nbt::compound::NbtCompound>>,
    /// Global server stopwatches
    pub stopwatches: std::sync::Mutex<crate::world::stopwatches::Stopwatches>,
    /// Global server random sequences
    pub random_sequences: std::sync::Mutex<crate::world::random_sequences::RandomSequences>,
    // Manages player advancement
    pub advancement_manager: Arc<AdvancementManager>,
    // Whether the server whitelist is on or off
    pub white_list: AtomicBool,
    /// Manages the server's tick rate, freezing, and sprinting
    pub tick_rate_manager: Arc<ServerTickRateManager>,
    /// Stores the duration of the last 100 ticks for performance analysis
    pub tick_times_nanos: std::sync::Mutex<[i64; 100]>,
    /// Aggregated tick times for efficient rolling average calculation
    pub aggregated_tick_times_nanos: AtomicI64,
    /// Total number of ticks processed by the server
    pub tick_count: AtomicI32,
    /// Owns the server-wide tick profiling session used by `/debug`.
    pub(crate) debug_profiler: debug_profiler::DebugProfiler,
    /// Random unique Server ID used by Bedrock Edition
    pub server_guid: u64,
    /// Player idle timeout in minutes (0 = disabled)
    pub player_idle_timeout: AtomicI32,
    /// Manages scheduled tasks (e.g. from plugins)
    /// Manages scheduled datapack functions (`/schedule`)
    pub scheduled_functions: Arc<crate::server::scheduler::ScheduledFunctionQueue>,
    tasks: TaskTracker,
    pub runtime: tokio::runtime::Handle,
    /// Dedicated Rayon thread pool for synchronous world and entity simulation (ticks).
    pub simulation_pool: Arc<rayon::ThreadPool>,
    /// Dedicated Rayon thread pool for background CPU computation (RSA, Bedrock crypto, compression).
    pub compute_pool: Arc<rayon::ThreadPool>,
    pub management_hub: Arc<crate::net::management::hub::ManagementHub>,
    storage_root: PathBuf,
    save_metadata_lock: Arc<tokio::sync::Mutex<()>>,
    storage_checkpoint: storage_checkpoint::StorageCheckpoint,
    /// Cancelled when this server should stop. A child of [`crate::STOP_INTERRUPT`], so a
    /// process-wide stop reaches it, while an integrated server can stop without ending the
    /// process.
    pub stop_token: CancellationToken,
    /// Serializes login commits against transfers and the closing of admission.
    admission_closed: std::sync::RwLock<bool>,
    network_sessions: Arc<std::sync::Mutex<rustc_hash::FxHashSet<uuid::Uuid>>>,
    /// Stores the latest available Pumpkin release if the startup update checker found a newer version.
    pub available_update: std::sync::OnceLock<crate::update_checker::GitHubRelease>,

    // world stuff which maybe should be put into a struct
    pub level_info: Arc<ArcSwap<LevelData>>,
    world_info_writer: Arc<dyn WorldInfoWriter>,
    world_operations: SegQueue<WorldOperation>,
    world_openings: world_openings::WorldOpenings,
}

impl Server {
    pub(crate) async fn bedrock_auth_keys(
        &self,
    ) -> Result<&(String, pumpkin_auth::jwt::Jwks), pumpkin_auth::jwt::AuthError> {
        let auth = &self.advanced_config.networking.bedrock.authentication;
        self.bedrock_oidc_keys
            .get_or_try_init(|| async {
                if !auth.enabled {
                    return Err(pumpkin_auth::jwt::AuthError::PublicKeyBuild(
                        "Bedrock OIDC authentication is disabled".into(),
                    ));
                }
                let (issuer, jwks) = pumpkin_auth::jwt::fetch_oidc_jwks(
                    auth.url.as_deref(),
                    auth.connect_timeout,
                    auth.read_timeout,
                )
                .await?;
                if jwks.keys.is_empty() {
                    return Err(pumpkin_auth::jwt::AuthError::PublicKeyBuild(
                        "Bedrock OIDC provider returned no signing keys".into(),
                    ));
                }
                Ok((issuer, jwks))
            })
            .await
    }

    #[expect(clippy::too_many_lines)]
    pub async fn new(
        basic_config: BasicConfiguration,
        mut advanced_config: AdvancedConfiguration,
        telemetry_config: TelemetryConfig,
        vanilla_data: VanillaData,
        plugin_loaders: Vec<Arc<dyn PluginLoader>>,
    ) -> Result<Arc<Self>, WorldInfoError> {
        let shared_resources = crate::resource_pack::prepare(&mut advanced_config).await?;
        let permission_manager = Arc::new(PermissionManager::new());
        // First register the default commands. After that, plugins can put in their own.
        let command_dispatcher = ArcSwap::from_pointee(default_dispatcher(
            &permission_manager,
            &advanced_config.commands,
        ));

        crate::command::set_broadcast_console_to_ops(
            advanced_config.commands.broadcast_console_to_ops,
        );

        let world_path = basic_config.get_world_path();
        let mut dimensions = vec![Dimension::OVERWORLD];
        if basic_config.allow_nether {
            dimensions.push(Dimension::THE_NETHER);
        }
        if basic_config.allow_end {
            dimensions.push(Dimension::THE_END);
        }
        // Validate every clock before level startup can convert terrain or write metadata.
        let clocks = dimensions
            .iter()
            .map(|dim| {
                let folder = LevelFolder::for_dimension(world_path.clone(), dim);
                WorldClockSnapshot::read(&folder.dim_folder)
            })
            .collect::<Result<Vec<_>, _>>()?;

        let block_registry = super::block::registry::default_registry();

        let level_info = match AnvilLevelInfo.read_world_info(&world_path) {
            Ok(level_info) => {
                let dat_path = world_path.join(LEVEL_DAT_FILE_NAME);
                if dat_path.exists() {
                    let backup_path = world_path.join(LEVEL_DAT_BACKUP_FILE_NAME);
                    if let Err(err) = fs::copy(&dat_path, &backup_path) {
                        warn!("Failed to create backup {LEVEL_DAT_BACKUP_FILE_NAME}: {err}");
                    }
                }
                level_info
            }
            Err(WorldInfoError::InfoNotFound) => {
                warn!(
                    "No {LEVEL_DAT_FILE_NAME} in {}, creating a new world with seed {}",
                    world_path.display(),
                    basic_config.seed.0 as i64
                );
                let seed = basic_config.seed;
                let mut default_data = match advanced_config.world.generator {
                    WorldPreset::Normal => {
                        let overworld_gen =
                            pumpkin_world::generation::generator::VanillaGenerator::new(
                                seed,
                                Dimension::OVERWORLD,
                            );
                        LevelData::from_world_generator(seed, &overworld_gen)
                    }
                    WorldPreset::Flat => {
                        let mut data = LevelData::default_with_preset(seed, "flat");
                        let overworld = Dimension::OVERWORLD;
                        let settings = data
                            .world_gen_settings
                            .dimensions
                            .get(overworld.minecraft_name)
                            .and_then(|dim| dim.generator.settings.as_ref())
                            .and_then(GeneratorSettings::as_flat_settings)
                            .ok_or_else(|| {
                                WorldInfoError::DeserializationError(
                                    "Could not load the built-in flat world preset".to_string(),
                                )
                            })?;
                        data.spawn_y =
                            overworld.min_y + settings.layers.iter().map(|l| l.height).sum::<i32>();
                        data
                    }
                };
                if basic_config.hardcore {
                    default_data.difficulty = Difficulty::Hard;
                    default_data.difficulty_locked = true;
                } else {
                    default_data.difficulty = basic_config.default_difficulty;
                }
                AnvilLevelInfo.write_world_info(&default_data, &world_path)?;
                default_data
            }
            Err(
                error @ (WorldInfoError::UnsupportedDataVersion(_)
                | WorldInfoError::UnsupportedLevelVersion(_)),
            ) => {
                error!("Failed to load world info!");
                error!("{error}");
                error!("Unsupported world version! See the logs for more info.");
                return Err(error);
            }
            Err(error) => {
                error!("Failed to load the world data in {}!", world_path.display());
                error!("{error}");
                error!(
                    "Refusing to continue: a default world would generate different terrain on top of the existing region files. Restore {LEVEL_DAT_FILE_NAME} from {LEVEL_DAT_BACKUP_FILE_NAME}, which also holds a copy of the world seed, or move the world folder aside to start a new world."
                );
                error!("Failed to load the world data! See the logs for more info.");
                return Err(error);
            }
        };

        let world_path = world_path.canonicalize()?;
        let players_dir = world_path.join("players");
        let player_data_storage = ServerPlayerData::new(
            players_dir.join("data"),
            Duration::from_secs(advanced_config.player_data.save_player_cron_interval),
            advanced_config.player_data.save_player_data,
        );
        player_data_storage.recover().map_err(|err| {
            WorldInfoError::IoError(std::io::Error::other(format!(
                "Failed to recover player data in {}: {err}",
                players_dir.display()
            )))
        })?;
        storage_checkpoint::recover(
            &world_path,
            &advanced_config.world.chunk,
            player_data_storage.checkpoint_storage(),
        )
        .await
        .map_err(|err| {
            WorldInfoError::IoError(std::io::Error::other(format!(
                "Failed to recover world checkpoint: {err}"
            )))
        })?;

        let mut level_info = level_info;
        if basic_config.hardcore {
            level_info.difficulty = Difficulty::Hard;
            level_info.difficulty_locked = true;
            if let Err(err) = AnvilLevelInfo.write_world_info(&level_info, &world_path) {
                error!("Failed to persist hardcore difficulty fix: {err}");
            }
        }

        let level_info = Arc::new(ArcSwap::new(Arc::new(level_info)));

        let listing = std::sync::Mutex::new(CachedStatus::new(
            &basic_config,
            &advanced_config.networking.java.motd,
            advanced_config.networking.java.max_players,
        ));
        let defaultgamemode = std::sync::Mutex::new(DefaultGamemode {
            gamemode: basic_config.default_gamemode,
        });
        let advancement_manager = Arc::new(AdvancementManager::new(
            players_dir.clone(),
            advanced_config.advancement.enabled,
            advanced_config.advancement.save_advancements,
        ));
        let white_list = AtomicBool::new(basic_config.white_list);

        let tick_rate_manager = Arc::new(ServerTickRateManager::new(basic_config.tps));

        info!(
            "Enabled dimensions: {:?}",
            dimensions
                .iter()
                .map(|d| d.minecraft_name)
                .collect::<Vec<_>>()
        );

        let management_settings = Arc::new(crate::net::management::hub::ManagementSettings::new(
            &basic_config,
            &advanced_config,
            &advanced_config.networking.management,
        ));
        let management_hub = Arc::new(crate::net::management::hub::ManagementHub::new(
            management_settings,
        ));

        let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        let simulation_pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                // Leave a worker available to another world while one runs a blocking hook.
                .num_threads(cpus.max(2))
                .thread_name(|i| format!("Simulation-{i}"))
                .build()
                .map_err(|e| WorldInfoError::IoError(std::io::Error::other(e)))?,
        );
        let compute_threads = (cpus / 2).clamp(2, 8);
        let compute_pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(compute_threads)
                .thread_name(|i| format!("Compute-{i}"))
                .build()
                .map_err(|e| WorldInfoError::IoError(std::io::Error::other(e)))?,
        );

        let server = Self {
            basic_config,
            advanced_config,
            shared_resources,
            telemetry_config,
            telemetry_sessions: crate::telemetry::TelemetrySessionTracker::default(),
            data: vanilla_data,
            plugin_manager: Arc::new(PluginManager::new(plugin_loaders)),
            permission_manager,
            container_id: 0.into(),
            recipe_manager: Arc::new(recipe::RecipeManager::new()),
            datapack_manager: Arc::new(crate::data::datapack::DatapackManager::new()),
            enchantment_manager: Arc::new(enchantment::EnchantmentManager::new()),
            map_id: level_info.load().map_id.into(),
            worlds: ArcSwap::from_pointee(vec![]),
            dimensions,
            command_dispatcher,
            block_registry,
            item_registry: super::item::items::default_registry(),
            key_store: OnceCell::new(),
            bedrock_oidc_keys: Arc::new(OnceCell::new()),
            listing,
            branding: CachedBranding::new(),
            bossbars: std::sync::Mutex::new(CustomBossbars::new()),
            map_manager: MapManager::new(),
            defaultgamemode,
            player_data_storage,
            command_storage: std::sync::Mutex::new(std::collections::HashMap::new()),
            stopwatches: std::sync::Mutex::new(crate::world::stopwatches::Stopwatches::new()),
            random_sequences: std::sync::Mutex::new(
                crate::world::random_sequences::RandomSequences::new(),
            ),
            advancement_manager,
            white_list,
            tick_rate_manager,
            tick_times_nanos: std::sync::Mutex::new([0; 100]),
            aggregated_tick_times_nanos: AtomicI64::new(0),
            tick_count: AtomicI32::new(0),
            debug_profiler: debug_profiler::DebugProfiler::new(),
            tasks: TaskTracker::new(),
            stop_token: crate::STOP_INTERRUPT.child_token(),
            admission_closed: std::sync::RwLock::new(false),
            network_sessions: Arc::default(),
            runtime: tokio::runtime::Handle::current(),
            simulation_pool,
            compute_pool,
            scheduled_functions: Arc::new(crate::server::scheduler::ScheduledFunctionQueue::new()),
            server_guid: rand::random(),
            player_idle_timeout: AtomicI32::new(0),
            mojang_public_keys: ArcSwap::from_pointee(Vec::new()),
            world_info_writer: Arc::new(AnvilLevelInfo),
            world_operations: SegQueue::new(),
            world_openings: world_openings::WorldOpenings::default(),
            level_info,
            management_hub,
            storage_root: world_path.clone(),
            save_metadata_lock: Arc::new(tokio::sync::Mutex::new(())),
            storage_checkpoint: storage_checkpoint::StorageCheckpoint::default(),
            available_update: std::sync::OnceLock::new(),
        };
        let server = Arc::new(server);

        // Fetch / generate keys in background tasks to avoid blocking startup
        let server_clone = server.clone();
        server.spawn_task(async move {
            let key_store = Arc::new(KeyStore::new());
            let _ = server_clone.key_store.set(key_store);
        });

        if server.basic_config.allow_chat_reports {
            let server_clone = server.clone();
            server.spawn_task(async move {
                let auth_config = server_clone
                    .advanced_config
                    .networking
                    .java
                    .authentication
                    .clone();
                let keys = fetch_mojang_public_keys(&auth_config)
                    .await
                    .unwrap_or_else(|e| {
                        error!("Failed to fetch Mojang keys: {e}");
                        Vec::new()
                    });
                server_clone.mojang_public_keys.store(Arc::new(keys));
            });
        }

        if server.advanced_config.networking.bedrock.online_mode
            && server
                .advanced_config
                .networking
                .bedrock
                .authentication
                .enabled
        {
            let server_clone = server.clone();
            server.spawn_task(async move {
                if let Err(error) = server_clone.bedrock_auth_keys().await {
                    warn!("Failed to fetch Bedrock OIDC keys: {error}");
                }
            });
        }

        let legacy_clocks = read_world_clocks(&world_path);
        let mut worlds_vec = Vec::new();
        for (dim, clock) in server.dimensions.iter().zip(clocks) {
            info!(
                "Loading {}",
                TextComponent::text(dim.minecraft_name.to_string())
                    .color_named(NamedColor::DarkGreen)
                    .to_pretty_console()
            );
            let clock = clock.unwrap_or_else(|| {
                let time_of_day = legacy_clocks.clocks.get(dim.minecraft_name).map_or_else(
                    || {
                        if dim.minecraft_name == Dimension::OVERWORLD.minecraft_name {
                            server.level_info.load().day_time
                        } else {
                            0
                        }
                    },
                    |clock| clock.total_ticks,
                );
                WorldClockSnapshot {
                    time_of_day,
                    ..WorldClockSnapshot::default()
                }
            });
            worlds_vec.push(server.open_world(
                world_path.clone(),
                server.level_info.clone(),
                dim.clone(),
                clock,
            ));
        }

        for world in &worlds_vec {
            let mut world_init_event =
                crate::plugin::api::events::world::world_init::WorldInitEvent::new(world.clone());
            server
                .plugin_manager
                .fire(&server, &mut world_init_event)
                .await;

            let mut world_load_event =
                crate::plugin::api::events::world::world_load::WorldLoadEvent::new(world.clone());
            server
                .plugin_manager
                .fire(&server, &mut world_load_event)
                .await;
        }

        server.worlds.store(Arc::new(worlds_vec));

        info!("All worlds loaded successfully.");

        let enabled_packs = server.level_info.load().data_packs.enabled.clone();
        server
            .datapack_manager
            .load_all(&world_path, &enabled_packs, &server.recipe_manager);

        let source = crate::command::CommandSender::Console.into_source(&server);
        let _ = server
            .datapack_manager
            .execute_function(&server, &source, "#minecraft:load");

        Ok(server)
    }

    /// Spawns a task associated with this server. All tasks spawned with this method are awaited
    /// when the server stops. This means tasks should complete in a reasonable (no looping) amount of time.
    pub fn spawn_task<F>(&self, task: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.tasks.spawn_on(task, &self.runtime)
    }

    pub fn get_world_from_dimension(&self, dimension: &Dimension) -> Arc<World> {
        let worlds = self.worlds.load();
        worlds
            .iter()
            .find(|w| w.dimension.minecraft_name == dimension.minecraft_name)
            .cloned()
            .or_else(|| worlds.first().cloned())
            .unwrap_or_else(|| {
                error!("No default world exists");
                std::process::exit(1);
            })
    }

    pub(crate) fn find_sibling_dimension(
        &self,
        source: &World,
        dimension: &Dimension,
    ) -> Option<Arc<World>> {
        self.worlds
            .load()
            .iter()
            .find(|world| {
                world.dimension.minecraft_name == dimension.minecraft_name
                    && Arc::ptr_eq(&world.level_info, &source.level_info)
                    && world.is_open()
            })
            .cloned()
    }

    /// Opens a plugin world.
    ///
    /// # Panics
    /// Panics if its saved clock cannot be read. Use [`Self::try_create_world`]
    /// to handle a storage error.
    #[expect(clippy::expect_used)]
    pub fn create_world(self: &Arc<Self>, name: String, dimension: Dimension) -> Arc<World> {
        self.try_create_world(name, dimension)
            .expect("Failed to create world")
    }

    pub fn try_create_world(
        self: &Arc<Self>,
        name: String,
        dimension: Dimension,
    ) -> Result<Arc<World>, WorldInfoError> {
        let root = &self.storage_root;
        // The primary world's existing name still addresses its root directly.
        let uses_primary_root = self.worlds.load().iter().any(|world| {
            world.level.level_folder.root_folder == *root && world.get_world_name() == name
        });
        let world_path = if uses_primary_root {
            root.clone()
        } else {
            root.join(name)
        };
        fs::create_dir_all(&world_path)?;
        let world_path = world_path.canonicalize()?;
        // A directory can admit several dimensions, with one metadata owner.
        let opening = self.world_openings.reserve(&world_path)?;
        let loaded = self.worlds.load_full();
        let mut siblings = loaded
            .iter()
            .filter(|world| world.level.level_folder.root_folder == world_path);
        let sibling = siblings.next();
        if let Some(world) = sibling
            .into_iter()
            .chain(siblings)
            .find(|world| world.dimension == dimension)
        {
            return if world.is_open() {
                Ok(world.clone())
            } else {
                Err(io::Error::new(io::ErrorKind::WouldBlock, "World is unloading").into())
            };
        }

        let folder = LevelFolder::for_dimension(world_path.clone(), &dimension);
        let clock = WorldClockSnapshot::read(&folder.dim_folder)?.unwrap_or_else(|| {
            let legacy = read_world_clocks(&world_path);
            WorldClockSnapshot {
                time_of_day: legacy
                    .clocks
                    .get(dimension.minecraft_name)
                    .map_or(0, |clock| clock.total_ticks),
                ..WorldClockSnapshot::default()
            }
        });
        let level_info = match sibling {
            Some(world) => world.level_info.clone(),
            None => self.load_independent_world_info(&world_path)?,
        };
        let world = self.open_world(world_path.clone(), level_info, dimension, clock);
        let _initializing = world.try_simulation();
        self.worlds.rcu(|worlds| {
            let mut new_worlds = (**worlds).clone();
            new_worlds.push(world.clone());
            new_worlds
        });
        // Plugin hooks may request the same world. Its owner and storage are
        // published before callbacks can attempt another opening.
        drop(opening);
        let mut event =
            crate::plugin::api::events::world::world_init::WorldInitEvent::new(world.clone());
        self.plugin_manager.fire_blocking(self, &mut event);
        Ok(world)
    }

    fn open_world(
        self: &Arc<Self>,
        world_path: PathBuf,
        level_info: Arc<ArcSwap<LevelData>>,
        dimension: Dimension,
        clock: WorldClockSnapshot,
    ) -> Arc<World> {
        let level = Level::with_checkpoint_fence(
            &self.advanced_config.world,
            world_path,
            level_info.load().world_gen_settings.seed,
            dimension.clone(),
            self.player_data_storage.checkpoint.clone(),
        );
        let mut world = World::load(
            level.clone(),
            level_info,
            dimension,
            self.block_registry.clone(),
            Arc::downgrade(self),
        );
        world.level_time = std::sync::Mutex::new(clock.into());
        let world = Arc::new(world);
        let portal: Arc<dyn WorldPortalExt> = Arc::new(WorldPortal(world.clone()));
        level.world_portal.store(Arc::new(Some(portal)));
        world
    }

    pub async fn unload_world(&self, name: &str) -> Result<(), String> {
        let worlds = self.worlds.load();
        let world_to_unload = worlds
            .iter()
            .find(|w| w.get_world_name() == name || w.dimension.minecraft_name == name)
            .cloned()
            .ok_or_else(|| format!("World '{name}' not found"))?;

        if let Some(first_world) = worlds.first()
            && Arc::ptr_eq(first_world, &world_to_unload)
        {
            return Err("Cannot unload the primary/default world".to_string());
        }

        let _reservation = world_to_unload
            .try_simulation()
            .ok_or_else(|| format!("World '{name}' is processing simulation work"))?;
        world_to_unload.check_player_admission_for_unload(name)?;
        self.save_independent_world_info(&world_to_unload)
            .await
            .map_err(|error| format!("Failed to save world '{name}': {error}"))?;
        world_to_unload.close_player_admission(name)?;
        world_to_unload.shutdown().await;
        world_to_unload.unload().await;

        self.worlds.rcu(|w_list| {
            let mut new_list = (**w_list).clone();
            new_list.retain(|w| !Arc::ptr_eq(w, &world_to_unload));
            new_list
        });

        Ok(())
    }

    pub fn reload_datapacks(&self, server: &Arc<Self>) {
        let enabled_packs = self.level_info.load().data_packs.enabled.clone();
        self.datapack_manager
            .load_all(&self.storage_root, &enabled_packs, &self.recipe_manager);

        let source = crate::command::CommandSender::Console.into_source(server);
        let _ = self
            .datapack_manager
            .execute_function(server, &source, "#minecraft:load");

        let dynamic_recipes = self.recipe_manager.get_dynamic_recipes_internal();
        for player in self.get_all_players() {
            if let crate::net::ClientPlatform::Java(java_client) = player.client.as_ref() {
                let add_packet = sculith_protocol::java::client::play::CRecipeBookAdd::new(
                    true,
                    &dynamic_recipes,
                );
                if let Ok(data) = java_client.serialize_packet(&add_packet) {
                    java_client.try_enqueue_packet(data);
                }
            }
        }
    }

    #[must_use]
    pub fn get_known_packs<'a>(
        &self,
        server_version: &'a str,
        loaded_packs: &'a [crate::data::datapack::LoadedDatapack],
    ) -> Vec<sculith_protocol::KnownPack<'a>> {
        self.datapack_manager
            .get_known_packs(self, server_version, loaded_packs)
    }

    #[must_use]
    pub fn get_enabled_features(&self) -> Vec<&'static str> {
        self.datapack_manager.get_enabled_features(self)
    }

    #[must_use]
    pub fn is_feature_enabled(&self, feature: &str) -> bool {
        self.datapack_manager.is_feature_enabled(self, feature)
    }

    async fn save_metadata(&self, save_players: bool) -> Result<(), String> {
        let save = self.save_metadata_lock.clone().lock_owned().await;
        let _save = self
            .save_world_info_in_background(save)
            .await
            .map_err(|err| {
                error!("Failed to save world info: {err}");
                format!("Failed to save world info: {err}")
            })?;

        if save_players && let Err(err) = self.player_data_storage.save_all_players(self).await {
            error!("Failed to save player data: {err}");
            return Err(format!("Failed to save player data: {err}"));
        }

        if let Err(err) = self
            .advancement_manager
            .save_all_players(&self.get_all_players())
            .await
        {
            error!("Failed to save player advancements: {err}");
            return Err(format!("Failed to save player advancements: {err}"));
        }
        Ok(())
    }

    pub async fn save_all(&self) -> Result<(), String> {
        self.management_hub.broadcast_server_saving();
        tokio::select! {
            result = self.finish_save(None) => result,
            () = self.player_data_storage.checkpoint.pending() => Err("A world checkpoint is in progress".to_string()),
        }
    }

    pub(crate) async fn save_with_checkpoint(self: &Arc<Self>) -> Result<(), String> {
        let server = self.clone();
        // A disconnected command sender must leave the writer running until recovery is safe.
        self.spawn_task(async move {
            let _save = server.storage_checkpoint.saves.lock().await;
            let _entity_changes = server.storage_checkpoint.entity_changes.write().await;
            server.management_hub.broadcast_server_saving();
            let covered = server.storage_checkpoint.save(&server).await?;
            server.finish_save(covered).await
        })
        .await
        .map_err(|err| format!("World checkpoint task failed: {err}"))?
    }

    pub(crate) fn try_entity_save_guard(&self) -> Option<tokio::sync::RwLockReadGuard<'_, ()>> {
        self.storage_checkpoint.entity_changes.try_read().ok()
    }

    async fn finish_save(
        &self,
        mut covered: Option<storage_checkpoint::SavedChunks>,
    ) -> Result<(), String> {
        self.save_metadata(covered.is_none()).await?;
        let mut failure = None;
        for world in self.worlds.load_full().iter() {
            let checkpoint_chunks = covered
                .as_mut()
                .and_then(|paths| paths.remove(&world.level.level_folder.dim_folder))
                .unwrap_or_default();
            if let Err(err) = world.save_after_checkpoint(checkpoint_chunks).await {
                let error = format!(
                    "Failed to save {} ({:?}): {err}",
                    world.get_world_name(),
                    world.dimension
                );
                error!("{error}");
                failure.get_or_insert(error);
            }
        }
        if let Some(err) = failure {
            return Err(err);
        }

        self.management_hub.broadcast_server_saved();

        Ok(())
    }

    /// Loads a saved player and resolves its world among the open world owners.
    async fn load_player_world(
        &self,
        client: &ClientPlatform,
        id: &uuid::Uuid,
    ) -> Option<(Arc<World>, Option<SavedPlayer>)> {
        let Some(access) = self.player_data_storage.checkpoint.read_ready().await else {
            client.try_kick(
                DisconnectReason::Unknown,
                &TextComponent::text("World storage recovery is pending"),
            );
            return None;
        };
        let data = self.player_data_storage.load_data(id);
        drop(access);
        let data = match data {
            Ok(data) => data,
            Err(err) => {
                error!("Failed to load player data for {id}: {err}");
                client.try_kick(
                    DisconnectReason::Unknown,
                    &TextComponent::text("Your player data could not be loaded"),
                );
                return None;
            }
        };
        let worlds = self.worlds.load();
        let first_world = worlds.first().cloned()?;
        let Some(mut data) = data else {
            return Some((first_world, None));
        };
        let dimension = data.get_string("Dimension").and_then(|key| {
            let dimension = Dimension::from_name(key);
            if dimension.is_none() {
                warn!("Invalid dimension key in player data: {key}");
            }
            dimension
        });
        let destination = match (data.get_string(SAVED_WORLD_ROOT), dimension) {
            (Some(root), Some(dimension)) => {
                let path = Path::new(root);
                worlds
                    .iter()
                    .find(|world| {
                        world.is_open()
                            && world.dimension == *dimension
                            && self.named_world_path(world) == Some(path)
                    })
                    .cloned()
            }
            (Some(_), None) => None,
            (None, Some(dimension)) => self.find_sibling_dimension(&first_world, dimension),
            (None, None) => Some(first_world.clone()),
        };
        let restore_position = destination.is_some();
        if !restore_position {
            warn!(player = %id, "Saved player world is unavailable, using the main world spawn");
            for field in ["Pos", "Motion", "Rotation", "RootVehicle"] {
                data.child_tags.remove(field);
            }
        }
        Some((
            destination.unwrap_or(first_world),
            Some(SavedPlayer {
                data,
                restore_position,
            }),
        ))
    }

    pub async fn add_player(
        self: &Arc<Self>,
        client: Arc<ClientPlatform>,
        profile: GameProfile,
        config: Option<PlayerConfig>,
    ) -> Option<(Arc<Player>, Arc<World>)> {
        let gamemode = self
            .defaultgamemode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .gamemode;

        let (world, nbt) = self.load_player_world(client.as_ref(), &profile.id).await?;

        let mut player = Player::new(
            client,
            profile,
            config.clone().unwrap_or_default(),
            &world,
            gamemode,
        );

        let initial_position = if let Some(mut saved) = nbt {
            player.read_nbt(&mut saved.data);
            // The data file itself proves this is a returning player. Older Bedrock
            // sessions could persist HasPlayedBefore as false and mask a valid Pos.
            player.has_played_before.store(true, Ordering::Relaxed);
            (!saved.restore_position).then(|| world_membership::InitialSpawn {
                position: player.position(),
                returned_from_unavailable_world: true,
            })
        } else {
            Some(world_membership::InitialSpawn {
                position: player.position(),
                returned_from_unavailable_world: false,
            })
        };

        // Wrap in Arc after data is loaded
        let player = Arc::new(player);
        // Joining already sends play packets, which must reach `PacketSentEvent` for translation.
        player.client.set_player(player.clone());
        let player_clone = player.clone();
        self.spawn_task(async move {
            let properties = player_clone.gameprofile.properties.load().clone();
            if let Some(mut skin) = Player::fetch_skin(&properties).await {
                if skin.skin_id == "Standard_Custom" {
                    let skin_id = format!("pumpkin:{}", player_clone.gameprofile.id);
                    skin.skin_id.clone_from(&skin_id);
                    skin.full_id = skin_id;
                }
                player_clone.bedrock_skin.store(Arc::new(skin));
            }
        });
        {
            let mut advancements = player
                .advancements
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(e) = advancements.load() {
                warn!("Error loading player {}: {e}", player.gameprofile.id);
            }
            advancements.player = Arc::downgrade(&player);
        };

        send_cancellable_blocking! {{
            self;
            &mut PlayerLoginEvent::new(player.clone(), TextComponent::text("You have been kicked from the server"));
            'after: {
                player.screen_handler_sync_handler.store_player(player.clone());
                if let Err(rejection) = self.request_player_admission(&world, &player, initial_position).await {
                    let (reason, message) = rejection.disconnect();
                    player.kick(reason, &message);
                    player.client.clear_player();
                    return None;
                }
                Some({
                    {
                        let mut user_cache = self
                            .data
                            .user_cache
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        user_cache.upsert(player.gameprofile.id, player.gameprofile.name.clone());
                    };

                    // TODO: Config if we want increase online
                    if let Some(config) = config {
                        // TODO: Config so we can also just ignore this hehe
                        if config.server_listing {
                            self.listing
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .add_player(&player);
                        }
                    }

                    let player_dto = crate::net::management::dto::PlayerDto::new(
                        Some(player.gameprofile.id),
                        Some(player.gameprofile.name.clone()),
                    );
                    self.management_hub.broadcast_player_joined(&player_dto);
                    if self.telemetry_config.enabled {
                        self.telemetry_sessions.record_join(player.gameprofile.id);
                    }

                    (player, world)
                })
            }

            'cancelled: {
                player.kick(DisconnectReason::Kicked, &event.kick_message);
                player.client.clear_player();
                None
            }
        }}
    }

    pub fn remove_player(&self, player: &Player) {
        if self.telemetry_config.enabled {
            self.telemetry_sessions.record_leave(player);
        }
        let player_dto = crate::net::management::dto::PlayerDto::new(
            Some(player.gameprofile.id),
            Some(player.gameprofile.name.clone()),
        );
        self.management_hub.broadcast_player_left(&player_dto);

        player.increment_stat(
            pumpkin_data::statistic::StatisticCategory::Custom,
            pumpkin_data::statistic::CustomStatistic::LeaveGame as i32,
            1,
        );
        // TODO: Config if we want decrease online
        self.listing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove_player(player);
    }

    /// Refuses all later joins. Returns once every join in progress is in a world
    pub fn close_admission(&self) {
        *self
            .admission_closed
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    }

    pub(crate) fn submit_world_transfer(&self, transfer: WorldTransfer) {
        self.submit_world_operation(WorldOperation::Transfer(Box::new(transfer)));
    }

    pub(crate) fn player_admission_state(&self) -> std::sync::RwLockReadGuard<'_, bool> {
        self.admission_closed
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn submit_world_operation(&self, change: WorldOperation) {
        let closed = self.player_admission_state();
        if !*closed && !self.stop_token.is_cancelled() {
            self.world_operations.push(change);
        }
    }

    fn for_each_ready_world_operation(
        &self,
        mut apply: impl FnMut(WorldOperation, WorldTransferReservation),
    ) {
        for _ in 0..self.world_operations.len().min(32) {
            let Some(change) = self.world_operations.pop() else {
                break;
            };
            if change.abandoned() {
                continue;
            }
            if let Some(ownership) = change.try_reserve_worlds() {
                apply(change, ownership);
            } else {
                self.world_operations.push(change);
            }
        }
    }

    pub(super) fn cancel_pending_simulation_changes(&self) {
        // Stop admission first so no request can be left waiting after the ticker exits.
        self.close_admission();
        while self.world_operations.pop().is_some() {}
        for world in self.worlds.load().iter() {
            // Let a regional claim publish its receipt before dropping unstarted requests.
            let _regions = self.runtime.block_on(world.region_activity.write());
            world.cancel_block_edits();
        }
    }

    /// Asks this server to stop: the ticker ends and a standalone server stops accepting
    /// connections and shuts down.
    pub fn stop(&self) {
        self.stop_token.cancel();
    }

    pub async fn shutdown(&self) {
        self.management_hub.broadcast_server_stopping();
        self.world_openings.close().await;

        self.tasks.close();
        debug!("Awaiting tasks for server");
        self.tasks.wait().await;
        debug!("Done awaiting tasks for server");

        info!("Starting worlds");
        for world in self.worlds.load().iter() {
            world.shutdown().await;
        }
        let save = self.save_metadata_lock.clone().lock_owned().await;
        if let Err(err) = self.save_world_info_in_background(save).await {
            error!("Failed to save level.dat: {err}");
        }
        info!("Completed worlds");
    }

    /// Broadcasts a packet to all players in all worlds.
    ///
    /// This function sends the specified packet to every connected player in every world managed by the server.
    ///
    /// # Arguments
    ///
    /// * `packet`: A reference to the packet to be broadcast. The packet must implement the `ClientPacket` trait.
    pub fn broadcast_packet_all<P: ClientPacket>(&self, packet: &P) {
        for world in self.worlds.load().iter() {
            world.broadcast_packet_all(packet);
        }
    }

    /// Broadcasts custom server links to all players on the server.
    pub fn broadcast_server_links(&self, links: &[sculith_protocol::Link<'_>]) {
        let packet = sculith_protocol::java::client::play::CPlayServerLinks::new(links);
        self.broadcast_packet_all(&packet);
    }

    pub fn broadcast_tab_list_header_footer(&self, header: &TextComponent, footer: &TextComponent) {
        let packet = CTabList::new(header, footer);
        for world in self.worlds.load().iter() {
            for player in world.players.load().iter() {
                *player
                    .tab_list_header
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = header.clone();
                *player
                    .tab_list_footer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = footer.clone();
            }
            world.broadcast_packet_all(&packet);
        }
    }

    pub fn broadcast_message(
        self: &Arc<Self>,
        message: &TextComponent,
        sender_name: &TextComponent,
        chat_type: u8,
        target_name: Option<&TextComponent>,
    ) {
        let mut event = ServerBroadcastEvent::new(message.clone(), sender_name.clone());
        self.plugin_manager.fire_blocking(self, &mut event);
        if !event.cancelled {
            for world in self.worlds.load().iter() {
                world.broadcast_message(&event.message, &event.sender, chat_type, target_name);
            }
        }
    }

    pub fn broadcast_chat_message(
        self: &Arc<Self>,
        message: &crate::net::chat::PlayerChatMessage,
        is_filtered: impl Fn(&crate::entity::player::Player) -> bool,
        sender_player: Option<&Arc<crate::entity::player::Player>>,
        chat_type: sculith_protocol::codec::var_int::VarInt,
        sender_name: &TextComponent,
        target_name: Option<&TextComponent>,
    ) {
        for world in self.worlds.load().iter() {
            world.broadcast_chat_message(
                message,
                &is_filtered,
                sender_player,
                chat_type,
                sender_name,
                target_name,
            );
        }
    }

    /// Gets the current difficulty of the server.
    pub fn get_difficulty(&self) -> Difficulty {
        self.level_info.load().difficulty
    }

    /// Sets the server-wide difficulty and publishes it to the loaded worlds.
    pub fn set_difficulty(&self, difficulty: Difficulty, force_update: bool) {
        let difficulty = self.effective_difficulty(difficulty);
        if !Self::update_difficulty(&self.level_info, difficulty, force_update) {
            return;
        }
        let worlds = self.worlds.load();
        for world in world_metadata::metadata_owners(&worlds) {
            if !Arc::ptr_eq(&world.level_info, &self.level_info) {
                world.set_difficulty(difficulty);
            }
        }
        for world in worlds.iter() {
            Self::publish_world_difficulty(world);
        }
    }

    /// Sets the server-wide difficulty lock and publishes it to the loaded worlds.
    pub fn set_difficulty_locked(&self, locked: bool) {
        Self::update_difficulty_lock(&self.level_info, locked);
        let worlds = self.worlds.load();
        for world in world_metadata::metadata_owners(&worlds) {
            if !Arc::ptr_eq(&world.level_info, &self.level_info) {
                Self::update_difficulty_lock(&world.level_info, locked);
            }
        }
        for world in worlds.iter() {
            Self::publish_world_difficulty(world);
        }
    }

    pub(crate) fn set_scoped_difficulty(
        &self,
        world: &World,
        difficulty: Difficulty,
        force_update: bool,
    ) -> bool {
        let difficulty = self.effective_difficulty(difficulty);
        if !Self::update_difficulty(&world.level_info, difficulty, force_update) {
            return false;
        }
        self.publish_difficulty_scope(world);
        true
    }

    pub(crate) fn set_scoped_difficulty_locked(&self, world: &World, locked: bool) {
        Self::update_difficulty_lock(&world.level_info, locked);
        self.publish_difficulty_scope(world);
    }

    const fn effective_difficulty(&self, difficulty: Difficulty) -> Difficulty {
        if self.basic_config.hardcore {
            Difficulty::Hard
        } else {
            difficulty
        }
    }

    fn update_difficulty(
        info: &ArcSwap<LevelData>,
        difficulty: Difficulty,
        force_update: bool,
    ) -> bool {
        let previous = info.rcu(|current| {
            if current.difficulty_locked && !force_update {
                return Arc::clone(current);
            }
            let mut updated = (**current).clone();
            updated.difficulty = difficulty;
            Arc::new(updated)
        });
        force_update || !previous.difficulty_locked
    }

    fn update_difficulty_lock(info: &ArcSwap<LevelData>, locked: bool) {
        info.rcu(|current| {
            let mut updated = (**current).clone();
            updated.difficulty_locked = locked;
            updated
        });
    }

    fn publish_difficulty_scope(&self, source: &World) {
        let worlds = self.worlds.load();
        for world in worlds.iter() {
            if Arc::ptr_eq(&world.level_info, &source.level_info) {
                Self::publish_world_difficulty(world);
            }
        }
    }

    fn publish_world_difficulty(world: &World) {
        let info = world.level_info.load();
        let difficulty = info.difficulty;
        world.broadcast_editioned(
            &CChangeDifficulty::new(difficulty as u8, info.difficulty_locked),
            &sculith_protocol::bedrock::client::CSetDifficulty {
                difficulty: (difficulty as u32).into(),
            },
        );
    }

    /// Searches for a player by their username across all worlds.
    ///
    /// This function iterates through each world managed by the server and attempts to find a player with the specified username.
    /// If a player is found in any world, it returns an `Arc<Player>` reference to that player. Otherwise, it returns `None`.
    ///
    /// # Arguments
    ///
    /// * `name`: The username of the player to search for.
    ///
    /// # Returns
    ///
    /// An `Option<Arc<Player>>` containing the player if found, or `None` if not found.
    pub fn get_player_by_name(&self, name: &str) -> Option<Arc<Player>> {
        for world in self.worlds.load().iter() {
            if let Some(player) = world.get_player_by_name(name) {
                return Some(player);
            }
        }
        None
    }

    pub fn get_players_by_ip(&self, ip: IpAddr) -> Vec<Arc<Player>> {
        let mut players = Vec::<Arc<Player>>::new();

        for world in self.worlds.load().iter() {
            for player in world.players.load().iter() {
                if player.client.address().ip() == ip {
                    players.push(player.clone());
                }
            }
        }

        players
    }

    /// Returns all players from all worlds.
    pub fn get_all_players(&self) -> Vec<Arc<Player>> {
        let mut players = Vec::<Arc<Player>>::new();

        for world in self.worlds.load().iter() {
            players.extend(world.players.load().iter().cloned());
        }

        players
    }

    pub fn for_each_player<F>(&self, mut f: F)
    where
        F: FnMut(&Arc<Player>),
    {
        let worlds = self.worlds.load();

        for world in worlds.iter() {
            let players = world.players.load();
            for player in players.iter() {
                f(player);
            }
        }
    }

    /// Returns a random player from any of the worlds, or `None` if all worlds are empty.
    pub fn get_random_player(&self) -> Option<Arc<Player>> {
        let players = self.get_all_players();
        players.choose(&mut rand::rng()).map(Arc::<_>::clone)
    }

    /// Searches for a player by their UUID across all worlds.
    ///
    /// This function iterates through each world managed by the server and attempts to find a player with the specified UUID.
    /// If a player is found in any world, it returns an `Arc<Player>` reference to that player. Otherwise, it returns `None`.
    ///
    /// # Arguments
    ///
    /// * `id`: The UUID of the player to search for.
    ///
    /// # Returns
    ///
    /// An `Option<Arc<Player>>` containing the player if found, or `None` if not found.
    pub fn get_player_by_uuid(&self, id: uuid::Uuid) -> Option<Arc<Player>> {
        for world in self.worlds.load().iter() {
            if let Some(player) = world.get_player_by_uuid(id) {
                return Some(player);
            }
        }
        None
    }

    /// Counts the total number of players across all worlds.
    ///
    /// This function iterates through each world and sums up the number of players currently connected to that world.
    ///
    /// # Returns
    ///
    /// The total number of players connected to the server.
    pub fn get_player_count(&self) -> usize {
        let mut count = 0;
        for world in self.worlds.load().iter() {
            count += world.players.load().len();
        }
        count
    }

    /// Similar to [`Server::get_player_count`] >= n, but may be more efficient since it stops its iteration through all worlds as soon as n players were found.
    pub fn has_n_players(&self, n: usize) -> bool {
        let mut count = 0;
        for world in self.worlds.load().iter() {
            count += world.players.load().len();
            if count >= n {
                return true;
            }
        }
        false
    }

    /// Returns the maximum number of players allowed on the server.
    #[must_use]
    pub const fn max_players(&self) -> u32 {
        self.advanced_config.networking.java.max_players
    }

    /// Starts the background telemetry task if enabled in configuration.
    pub fn start_telemetry(self: &Arc<Self>) {
        crate::telemetry::start_telemetry(self.clone());
    }

    /// Starts the background update checker task if enabled in configuration.
    pub fn start_update_checker(self: &Arc<Self>) {
        crate::update_checker::start_update_checker(self);
    }

    /// Notifies a joining player in chat if they are an operator and a Pumpkin update is available.
    pub fn notify_update_if_op(&self, player: &Player) {
        crate::update_checker::notify_update_if_op(self, player);
    }

    /// Generates a new container id.
    pub fn new_container_id(&self) -> u32 {
        self.container_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Generates a new map id.
    pub fn next_map_id(&self) -> i32 {
        let id = self.map_id.fetch_add(1, Ordering::SeqCst);
        self.level_info.rcu(|level_info| {
            let mut new_level_info = (**level_info).clone();
            new_level_info.map_id = self.map_id.load(Ordering::SeqCst);
            new_level_info
        });
        id
    }

    pub const fn get_branding(&self) -> CPluginMessage<'_> {
        self.branding.get_branding()
    }

    pub const fn get_status(&self) -> &std::sync::Mutex<CachedStatus> {
        &self.listing
    }

    async fn get_or_init_key_store(&self) -> &Arc<KeyStore> {
        self.key_store
            .get_or_init(|| async {
                let (tx, rx) = tokio::sync::oneshot::channel();
                self.compute_pool.spawn(move || {
                    let _ = tx.send(Arc::new(KeyStore::new()));
                });
                rx.await.unwrap_or_else(|_| Arc::new(KeyStore::new()))
            })
            .await
    }

    pub async fn encryption_request<'a>(
        &'a self,
        verification_token: &'a [u8; 4],
        should_authenticate: bool,
    ) -> CEncryptionRequest<'a> {
        self.get_or_init_key_store().await.encryption_request(
            "",
            verification_token,
            should_authenticate,
        )
    }

    pub async fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>, EncryptionError> {
        let key_store = self.get_or_init_key_store().await.clone();
        let data = data.to_vec();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.compute_pool.spawn(move || {
            let _ = tx.send(key_store.decrypt(&data));
        });
        rx.await.map_err(|_| EncryptionError::FailedDecrypt)?
    }

    pub fn digest_secret(&self, secret: &[u8]) -> String {
        self.key_store.get().map_or_else(
            || KeyStore::new().get_digest(secret),
            |key_store| key_store.get_digest(secret),
        )
    }

    /// A coordinated tick used by the server-wide step, sprint and legacy tick hooks.
    pub fn tick(self: &Arc<Self>) {
        let commands = PlayerCommands::default();
        self.tick_with_player_commands(&commands, |server| server.player_data_storage.tick(server));
        commands.run_all(self);
    }

    fn tick_with_player_commands(
        self: &Arc<Self>,
        commands: &PlayerCommands,
        autosave: impl FnOnce(&Self) + Send,
    ) {
        self.for_each_ready_world_operation(|change, reservation| {
            change.apply(self, reservation);
        });
        self.simulation_pool.install(|| {
            if self.tick_rate_manager.runs_normally() || self.tick_rate_manager.is_sprinting() {
                self.tick_worlds_with_player_commands(commands);
                autosave(self);
            } else {
                self.sync_game_time();
                self.tick_network_with_player_commands(commands);
            }
        });
        commands.run_pending(self);
        self.storage_checkpoint
            .capture_at_tick_boundary(self, CheckpointFence::try_write);
    }

    /// Runs networking without advancing the worlds' simulation clocks.
    pub fn tick_players_and_network(self: &Arc<Self>) {
        let commands = PlayerCommands::default();
        self.tick_network_with_player_commands(&commands);
        commands.run_all(self);
    }

    fn tick_network_with_player_commands(self: &Arc<Self>, commands: &PlayerCommands) {
        for world in self.worlds.load().iter() {
            if let Some(ownership) = world.try_simulation() {
                self.tick_owned_world(
                    world,
                    ownership,
                    WorldStep::NetworkOnly(None),
                    false,
                    commands,
                );
            }
        }
    }

    /// Runs the coordinated game tick used by legacy global hooks and operations.
    pub fn tick_worlds(self: &Arc<Self>) {
        let commands = PlayerCommands::default();
        self.tick_worlds_with_player_commands(&commands);
        self.player_data_storage.tick(self);
        commands.run_all(self);
    }

    fn tick_worlds_with_player_commands(self: &Arc<Self>, commands: &PlayerCommands) {
        let source = crate::command::CommandSender::Console
            .into_source(self)
            .with_silent();
        let _ = self
            .datapack_manager
            .execute_function(self, &source, "#minecraft:tick");
        self.plugin_manager.tick_loaders(self);
        self.scheduled_functions
            .tick(self, self.tick_count.load(Ordering::Relaxed) as u64);
        self.sync_game_time();

        let worlds = self.worlds.load();
        worlds.par_iter().for_each(|world| {
            if let Some(ownership) = world.try_simulation() {
                self.tick_owned_world(world, ownership, WorldStep::Coordinated, false, commands);
            }
        });
    }

    fn tick_owned_world(
        self: &Arc<Self>,
        world: &Arc<World>,
        _ownership: tokio::sync::OwnedMutexGuard<()>,
        mode: WorldStep,
        sync_time: bool,
        commands: &PlayerCommands,
    ) {
        if self.stop_token.is_cancelled() || !world.is_open() {
            return;
        }
        let _flushing = SuspendedPlayerFlushes::for_world(world);
        if sync_time {
            world.synchronize_local_time();
        }
        world.apply_block_edits();
        world.flush_regional_entity_updates();
        match mode {
            WorldStep::Coordinated => world.tick_simulation(self, None, Some(commands)),
            WorldStep::RegionalDeadlines(deadline) => {
                world.tick_simulation(self, Some(deadline), Some(commands));
            }
            WorldStep::NetworkOnly(deadline) => {
                // Players can move while frozen. Resume from a fresh simulation extent.
                world.invalidate_region_layout();
                world.flush_block_updates();
                world.flush_synced_block_events();
                let players = world.players.load();
                world.process_player_packets(&players, Some(commands), false);
                players
                    .par_iter()
                    .filter(|player| player.can_simulate_in(world))
                    .for_each(|player| player.tick_with_deadline(self, deadline));
            }
        }
        world.apply_entity_chunk_changes();
        world.flush_regional_entity_updates();
        world.flush_block_updates();
    }

    /// Vanilla `tickChildren` "timeSync": every 20 ticks, also while frozen.
    /// Sync before `tickTime()`. `tick_count + 1` is this vanilla tick.
    fn sync_game_time(&self) {
        if self.tick_count.load(Ordering::Relaxed).wrapping_add(1) % 20 == 0
            && let Some(overworld) =
                self.worlds.load().iter().find(|world| {
                    world.dimension.minecraft_name == Dimension::OVERWORLD.minecraft_name
                })
        {
            overworld.force_game_time_synchronization(self);
        }
    }

    /// Updates the tick time statistics with the duration of the last tick.
    pub fn update_tick_times(&self, tick_duration_nanos: i64) {
        let tick_count = self.tick_count.fetch_add(1, Ordering::Relaxed);
        let index = (tick_count % 100) as usize;

        let mut tick_times = self
            .tick_times_nanos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let old_time = tick_times[index];
        tick_times[index] = tick_duration_nanos;
        drop(tick_times);

        self.aggregated_tick_times_nanos
            .fetch_add(tick_duration_nanos - old_time, Ordering::Relaxed);

        let target_tick_nanos = self.tick_rate_manager.nanoseconds_per_tick();
        let idle_nanos = target_tick_nanos.saturating_sub(tick_duration_nanos);

        let sample_slice = [
            tick_duration_nanos.max(target_tick_nanos),
            tick_duration_nanos,
            0,
            idle_nanos,
        ];
        let packet = sculith_protocol::java::client::play::CDebugSample::new(
            &sample_slice,
            sculith_protocol::codec::var_int::VarInt(0),
        );
        for world in self.worlds.load().iter() {
            let players = world.players.load();
            let recipients = players
                .iter()
                .filter(|player| {
                    player.subscribed_debug_sample.load(Ordering::Relaxed)
                        && player.permission_lvl.load() >= sculith_util::PermissionLvl::Two
                })
                .filter_map(|player| player.client.java());
            World::broadcast_java_clients(&packet, recipients);
        }
    }

    /// Gets the rolling average tick time over the last 100 ticks, in nanoseconds.
    pub fn get_average_tick_time_nanos(&self) -> i64 {
        let tick_count = self.tick_count.load(Ordering::Relaxed);
        let sample_size = (tick_count as usize).min(100);
        if sample_size == 0 {
            return 0;
        }
        self.aggregated_tick_times_nanos.load(Ordering::Relaxed) / sample_size as i64
    }

    /// Returns the average Milliseconds Per Tick (MSPT).
    pub fn get_mspt(&self) -> f64 {
        let avg_nanos = self.get_average_tick_time_nanos();
        // Convert nanoseconds to decimal milliseconds
        avg_nanos as f64 / 1_000_000.0
    }

    /// Returns the Ticks Per Second (TPS).
    pub fn get_tps(&self) -> f64 {
        let mspt = self.get_mspt();
        if mspt <= 0.0 {
            return 0.0;
        }
        1000.0 / mspt
    }

    /// Returns a copy of the last 100 tick times.
    pub fn get_tick_times_nanos_copy(&self) -> [i64; 100] {
        *self
            .tick_times_nanos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub async fn register_service(self: &Arc<Self>, service_name: String) {
        let mut service_event =
            crate::plugin::api::events::server::service_register::ServiceRegisterEvent::new(
                service_name,
            );
        self.plugin_manager.fire(self, &mut service_event).await;
    }

    pub async fn unregister_service(self: &Arc<Self>, service_name: String) {
        let mut service_event =
            crate::plugin::api::events::server::service_unregister::ServiceUnregisterEvent::new(
                service_name,
            );
        self.plugin_manager.fire(self, &mut service_event).await;
    }

    pub async fn tab_complete(self: &Arc<Self>, buffer: String, completions: Vec<String>) {
        let mut tab_event = crate::plugin::api::events::server::tab_complete::TabCompleteEvent::new(
            buffer,
            completions,
        );
        self.plugin_manager.fire(self, &mut tab_event).await;
    }

    pub async fn enable_plugin(self: &Arc<Self>, plugin_name: String) {
        let mut enable_event =
            crate::plugin::api::events::server::plugin_enable::PluginEnableEvent::new(plugin_name);
        self.plugin_manager.fire(self, &mut enable_event).await;
    }

    pub async fn disable_plugin(self: &Arc<Self>, plugin_name: String) {
        let mut disable_event =
            crate::plugin::api::events::server::plugin_disable::PluginDisableEvent::new(
                plugin_name,
            );
        self.plugin_manager.fire(self, &mut disable_event).await;
    }
}
