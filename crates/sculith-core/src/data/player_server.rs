use crate::{
    entity::{NBTStorage, player::Player},
    server::Server,
};
use crossbeam::atomic::AtomicCell;
use pumpkin_inventory::screen_handler::ScreenHandler;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_world::data::{
    checkpoint::CheckpointFence,
    player_data::{PlayerDataError, PlayerDataStorage},
};
use rustc_hash::FxHashSet;
use std::sync::Arc;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::{
    runtime::Handle,
    sync::{Mutex, OwnedMutexGuard, OwnedRwLockReadGuard},
    task::JoinHandle,
};
use tracing::{debug, error};
use uuid::Uuid;

pub(crate) const SAVED_WORLD_ROOT: &str = "sculith:world";

/// Helper for managing player data in the server context.
///
/// This struct provides server-wide access to the `PlayerDataStorage` and
/// convenience methods for player handling.
pub struct ServerPlayerData {
    storage: Arc<PlayerDataStorage>,
    pub(crate) checkpoint: Arc<CheckpointFence>,
    /// Orders capture and persistence together, including blocking writes whose callers leave.
    save_order: Arc<Mutex<()>>,
    save_interval: Duration,
    last_save: AtomicCell<Instant>,
}

impl ServerPlayerData {
    /// Creates a new `ServerPlayerData` with specified configuration.
    pub fn new(data_path: impl Into<PathBuf>, save_interval: Duration, enabled: bool) -> Self {
        Self {
            storage: Arc::new(PlayerDataStorage::new(data_path, enabled)),
            checkpoint: Arc::default(),
            save_order: Arc::new(Mutex::new(())),
            save_interval,
            last_save: AtomicCell::new(Instant::now()),
        }
    }

    /// Handles a player leaving the server.
    ///
    /// This function saves player data when they disconnect.
    ///
    /// # Arguments
    ///
    /// * `player` - The player who left.
    ///
    /// # Returns
    ///
    /// A Result indicating success or the error that occurred.
    pub async fn handle_player_leave(&self, player: &Arc<Player>) -> Result<(), PlayerDataError> {
        Self::close_leave_inventory(player);
        self.extract_data_and_save_player(player).await
    }

    pub(crate) fn close_leave_inventory(player: &Player) {
        player
            .player_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .on_closed(player);
        player.on_handled_screen_closed();
    }

    /// Performs periodic maintenance tasks.
    ///
    /// This function is called synchronously on the server tick loop to check
    /// if it is time to save player data.
    pub fn tick(&self, server: &Server) {
        self.try_periodic_save(server, true, false, |_| Self::capture_online(server));
    }

    /// Captures available world owners and retries the busy worlds at the next boundary.
    pub(crate) fn tick_worlds_at_boundary(
        &self,
        server: &Server,
        deferred: &mut FxHashSet<Uuid>,
        start_cycle: bool,
    ) {
        self.try_periodic_save(server, start_cycle, !deferred.is_empty(), |due| {
            let mut snapshots = Vec::new();
            let mut retry = FxHashSet::default();
            for world in server.worlds.load().iter() {
                if !due && !deferred.contains(&world.uuid) {
                    continue;
                }
                if world.players.load().is_empty() {
                    continue;
                }
                let Some(_owner) = world.try_simulation() else {
                    retry.insert(world.uuid);
                    continue;
                };
                if world.is_open() {
                    snapshots.extend(
                        world
                            .players
                            .load()
                            .iter()
                            .map(|player| Self::capture(player)),
                    );
                }
            }
            *deferred = retry;
            snapshots
        });
    }

    fn try_periodic_save(
        &self,
        server: &Server,
        start_cycle: bool,
        has_deferred: bool,
        capture: impl FnOnce(bool) -> Vec<(Uuid, NbtCompound)>,
    ) {
        let now = Instant::now();
        let due = start_cycle && now.duration_since(self.last_save.load()) >= self.save_interval;
        if !self.storage.is_save_enabled() || (!due && !has_deferred) {
            return;
        }
        // Leave at most one captured batch in flight. Retry the next tick if a save is busy.
        let Ok(order) = self.save_order.clone().try_lock_owned() else {
            return;
        };
        let Some(access) = self.checkpoint.try_read() else {
            return;
        };
        let snapshots = capture(due);
        if due {
            self.last_save.store(now);
        }
        if snapshots.is_empty() {
            return;
        }
        let write = self.write_batch(&server.runtime, order, access, snapshots);
        server.spawn_task(async move {
            match Self::finish_write(write).await {
                Ok(()) => debug!("Periodic player data save completed"),
                Err(err) => error!("Failed to save periodic player data: {err}"),
            }
        });
    }

    /// Saves all players' data immediately.
    ///
    /// This function immediately saves all online players' data to disk.
    /// Useful for server shutdown or backup operations.
    pub async fn save_all_players(&self, server: &Server) -> Result<(), PlayerDataError> {
        if !self.storage.is_save_enabled() {
            return Ok(());
        }
        let order = self.save_order.clone().lock_owned().await;
        let access = self
            .checkpoint
            .read_ready()
            .await
            .ok_or_else(unresolved_checkpoint)?;
        let snapshots = Self::capture_online(server);
        Self::finish_write(self.write_batch(&server.runtime, order, access, snapshots)).await
    }

    /// Loads player data and applies it to a player.
    ///
    /// This function loads a player's data and applies it to their Player instance.
    /// For new players, it creates default data without errors.
    ///
    /// # Arguments
    ///
    /// * `player` - The player to load data for and apply to.
    ///
    /// # Returns
    ///
    /// A Result indicating success or the error that occurred.
    pub fn load_data(&self, uuid: &Uuid) -> Result<Option<NbtCompound>, PlayerDataError> {
        let (found, data) = self.storage.load_player_data(uuid)?;
        Ok(found.then_some(data))
    }

    /// Finishes the previous process's committed batch before a player can join.
    pub fn recover(&self) -> Result<(), PlayerDataError> {
        self.storage.recover()
    }

    pub(crate) fn checkpoint_storage(&self) -> Arc<PlayerDataStorage> {
        self.storage.clone()
    }

    pub(crate) fn is_save_enabled(&self) -> bool {
        self.storage.is_save_enabled()
    }

    /// Extracts and saves data from a player.
    ///
    /// This function extracts NBT data from a player and saves it to disk.
    ///
    /// # Arguments
    ///
    /// * `player` - The player to extract and save data for.
    ///
    /// # Returns
    ///
    /// A Result indicating success or the error that occurred.
    pub async fn extract_data_and_save_player(
        &self,
        player: &Player,
    ) -> Result<(), PlayerDataError> {
        self.save_player_snapshot(|| Self::capture(player)).await
    }

    pub(crate) async fn save_player_snapshot(
        &self,
        capture: impl FnOnce() -> (Uuid, NbtCompound),
    ) -> Result<(), PlayerDataError> {
        if !self.storage.is_save_enabled() {
            return Ok(());
        }

        let order = self.save_order.clone().lock_owned().await;
        let access = self
            .checkpoint
            .read_ready()
            .await
            .ok_or_else(unresolved_checkpoint)?;
        let snapshots = vec![capture()];
        Self::finish_write(self.write_batch(&Handle::current(), order, access, snapshots)).await
    }

    pub(crate) fn capture_online(server: &Server) -> Vec<(Uuid, NbtCompound)> {
        let mut snapshots = Vec::new();
        server.for_each_player(|player| snapshots.push(Self::capture(player)));
        snapshots
    }

    pub(crate) fn capture(player: &Player) -> (Uuid, NbtCompound) {
        let mut nbt = NbtCompound::new();
        player.write_nbt(&mut nbt);
        (player.gameprofile.id, nbt)
    }

    fn write_batch(
        &self,
        runtime: &Handle,
        order: OwnedMutexGuard<()>,
        access: OwnedRwLockReadGuard<()>,
        snapshots: Vec<(Uuid, NbtCompound)>,
    ) -> JoinHandle<Result<(), PlayerDataError>> {
        let storage = self.storage.clone();
        runtime.spawn_blocking(move || {
            let _order = order;
            let _access = access;
            storage.save_batch(snapshots)
        })
    }

    async fn finish_write(
        write: JoinHandle<Result<(), PlayerDataError>>,
    ) -> Result<(), PlayerDataError> {
        write.await.map_err(std::io::Error::other)?
    }
}

fn unresolved_checkpoint() -> PlayerDataError {
    std::io::Error::other("a world checkpoint is awaiting recovery").into()
}

#[cfg(test)]
mod test {
    use crate::data::player_server::ServerPlayerData;
    use pumpkin_nbt::compound::NbtCompound;
    use pumpkin_world::data::player_data::PlayerDataStorage;
    use std::time::Duration;
    use std::time::Instant;
    use tempfile::tempdir;
    use uuid::Uuid;

    #[tokio::test]
    async fn player_data_storage_new() {
        // Create a temporary directory for testing
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let storage = PlayerDataStorage::new(path.clone(), true);

        assert_eq!(storage.get_data_path().as_path(), path.as_path());
        // Note: save_enabled might be configured differently in your actual code
    }

    #[tokio::test]
    async fn player_data_storage_get_player_data_path() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let storage = PlayerDataStorage::new(path.clone(), true);

        let uuid = Uuid::new_v4();
        let expected_path = path.join(format!("{uuid}.dat"));

        assert_eq!(storage.get_player_data_path(&uuid), expected_path);
    }

    #[tokio::test]
    async fn player_data_storage_save_and_load() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let storage = PlayerDataStorage::new(path, true); // Ensure saving is enabled for this test

        let uuid = Uuid::new_v4();

        // Create test data
        let mut nbt = NbtCompound::new();
        nbt.put_string("TestKey", "TestValue".to_string());
        nbt.put_int("TestInt", 42);

        // Save the data
        storage.save_player_data(&uuid, nbt).unwrap();

        // Load the data
        let (load_success, loaded_nbt) = storage.load_player_data(&uuid).unwrap();

        assert!(load_success);
        assert_eq!(loaded_nbt.get_string("TestKey").unwrap(), "TestValue");
        assert_eq!(loaded_nbt.get_int("TestInt").unwrap(), 42);
    }

    #[tokio::test]
    async fn player_data_storage_load_nonexistent() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let storage = PlayerDataStorage::new(path, true); // Ensure saving is enabled for this test

        let uuid = Uuid::new_v4();

        // Try to load non-existent data
        let (load_success, empty_nbt) = storage.load_player_data(&uuid).unwrap();

        assert!(!load_success);
        assert_eq!(empty_nbt.child_tags.len(), 0);
    }

    #[tokio::test]
    async fn player_data_storage_disabled() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let storage = PlayerDataStorage::new(path, false);

        let uuid = Uuid::new_v4();
        let mut nbt = NbtCompound::new();
        nbt.put_string("TestKey", "TestValue".to_string());

        // Save should succeed but do nothing
        let save_result = storage.save_player_data(&uuid, nbt);
        assert!(save_result.is_ok());

        // Load should return empty data
        let (load_success, empty_nbt) = storage.load_player_data(&uuid).unwrap();
        assert!(!load_success);
        assert_eq!(empty_nbt.child_tags.len(), 0);
    }

    #[tokio::test]
    async fn server_player_data_new() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();
        let save_interval = Duration::from_mins(5);

        let player_data = ServerPlayerData::new(path, save_interval, true);

        assert_eq!(player_data.save_interval, save_interval);
        assert!(
            Instant::now().duration_since(player_data.last_save.load()) < Duration::from_secs(1)
        );
    }

    #[tokio::test]
    async fn player_data_file_structure() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let uuid = Uuid::new_v4();
        let storage = PlayerDataStorage::new(path, true);

        // Create and save player data
        let mut nbt = NbtCompound::new();
        nbt.put_string("name", "TestPlayer".to_string());
        nbt.put_int("level", 42);
        storage.save_player_data(&uuid, nbt).unwrap();

        // Verify the file exists
        let player_data_path = storage.get_player_data_path(&uuid);
        assert!(player_data_path.exists());

        // Load it again and verify content
        let (success, loaded_data) = storage.load_player_data(&uuid).unwrap();
        assert!(success);
        assert_eq!(loaded_data.get_string("name").unwrap(), "TestPlayer");
        assert_eq!(loaded_data.get_int("level").unwrap(), 42);
    }
}
