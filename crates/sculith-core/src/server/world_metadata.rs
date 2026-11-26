use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use arc_swap::ArcSwap;
use pumpkin_data::dimension::Dimension;
use pumpkin_world::{
    level::LevelFolder,
    world_info::{
        LevelData, WorldGenSettings, WorldInfoError, WorldInfoReader, WorldInfoWriter,
        anvil::AnvilLevelInfo,
        data_files::{minecraft_data_dir, read_world_clocks, read_world_gen_settings},
    },
};
use rustc_hash::FxHashSet;
use sculith_util::{Difficulty, world_seed::Seed};
use tokio::sync::OwnedMutexGuard;
use tracing::error;

use crate::world::World;

use super::Server;

impl Server {
    pub(crate) fn named_world_path<'a>(&self, world: &'a World) -> Option<&'a Path> {
        world
            .level
            .level_folder
            .root_folder
            .strip_prefix(&self.storage_root)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
    }

    pub(super) fn load_independent_world_info(
        &self,
        root: &Path,
    ) -> Result<Arc<ArcSwap<LevelData>>, WorldInfoError> {
        let mut info = match AnvilLevelInfo.read_world_info(root) {
            Ok(info) => info,
            Err(WorldInfoError::InfoNotFound) => {
                let primary = self.level_info.load();
                let mut info = (**primary).clone();
                info.clear_weather_time = -1;
                info.weather = None;
                let overworld =
                    LevelFolder::for_dimension(root.to_path_buf(), &Dimension::OVERWORLD);
                let data = minecraft_data_dir(root);
                if data.join("game_rules.dat").exists() {
                    return Err(WorldInfoError::DeserializationError(format!(
                        "Missing level.dat for the saved game rules in {}",
                        root.display()
                    )));
                }
                info.world_gen_settings = match read_world_gen_settings(root) {
                    Some(settings) => settings,
                    None if data.join("world_gen_settings.dat").exists()
                        || minecraft_data_dir(&overworld.dim_folder)
                            .join("world_gen_settings.dat")
                            .exists() =>
                    {
                        return Err(WorldInfoError::DeserializationError(format!(
                            "Failed to read the world generator in {}",
                            root.display()
                        )));
                    }
                    // Older plugin worlds without generation metadata used the normal generator.
                    None => {
                        WorldGenSettings::new(Seed(info.world_gen_settings.seed.cast_unsigned()))
                    }
                };
                info.day_time = read_world_clocks(root)
                    .clocks
                    .get(Dimension::OVERWORLD.minecraft_name)
                    .map_or(0, |clock| clock.total_ticks);
                if let Some(name) = root.file_name().and_then(|name| name.to_str()) {
                    name.clone_into(&mut info.level_name);
                }
                self.world_info_writer.write_world_info(&info, root)?;
                info
            }
            Err(error) => return Err(error),
        };
        if self.basic_config.hardcore {
            info.difficulty = Difficulty::Hard;
            info.difficulty_locked = true;
        }
        Ok(Arc::new(ArcSwap::from_pointee(info)))
    }

    /// Saves one level metadata file per loaded world directory.
    pub fn save_world_info(&self) -> Result<(), WorldInfoError> {
        let _save = self.save_metadata_lock.try_lock().map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "World metadata is already being saved",
            )
        })?;
        write_snapshots(self.world_info_writer.as_ref(), self.world_info_snapshots())
    }

    pub(super) async fn save_world_info_in_background(
        &self,
        save: OwnedMutexGuard<()>,
    ) -> Result<OwnedMutexGuard<()>, WorldInfoError> {
        save_snapshots(
            self.world_info_writer.clone(),
            self.world_info_snapshots(),
            save,
        )
        .await
    }

    pub(super) async fn save_independent_world_info(
        &self,
        world: &World,
    ) -> Result<(), WorldInfoError> {
        if Arc::ptr_eq(&world.level_info, &self.level_info) {
            return Ok(());
        }
        let save = self.save_metadata_lock.clone().lock_owned().await;
        let snapshot = {
            let worlds = self.worlds.load();
            let owner = metadata_owners(&worlds)
                .find(|candidate| Arc::ptr_eq(&world.level_info, &candidate.level_info))
                .unwrap_or(world);
            metadata_snapshot(owner)
        };
        save_snapshots(self.world_info_writer.clone(), [snapshot], save)
            .await
            .map(drop)
    }

    fn world_info_snapshots(&self) -> Vec<(PathBuf, LevelData)> {
        let worlds = self.worlds.load();
        let mut main = (**self.level_info.load()).clone();
        if let Some(overworld) = worlds.first() {
            copy_overworld_state(overworld, &mut main);
        }
        std::iter::once((self.storage_root.clone(), main))
            .chain(
                metadata_owners(&worlds)
                    .filter(|world| !Arc::ptr_eq(&world.level_info, &self.level_info))
                    .map(metadata_snapshot),
            )
            .collect()
    }
}

pub(super) fn metadata_owners(worlds: &[Arc<World>]) -> impl Iterator<Item = &World> {
    let mut seen = FxHashSet::default();
    worlds
        .iter()
        .filter(|world| world.dimension == Dimension::OVERWORLD)
        .chain(
            worlds
                .iter()
                .filter(|world| world.dimension != Dimension::OVERWORLD),
        )
        .filter(move |world| seen.insert(Arc::as_ptr(&world.level_info)))
        .map(Arc::as_ref)
}

fn metadata_snapshot(world: &World) -> (PathBuf, LevelData) {
    let mut info = (**world.level_info.load()).clone();
    if world.dimension == Dimension::OVERWORLD {
        copy_overworld_state(world, &mut info);
    }
    (world.level.level_folder.root_folder.clone(), info)
}

fn copy_overworld_state(world: &World, info: &mut LevelData) {
    info.day_time = world.get_time_of_day();
    let state = world
        .weather
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .saved_state(info.data_version);
    info.clear_weather_time = state.clear_weather_time;
    info.weather = Some(state);
}

async fn save_snapshots(
    writer: Arc<dyn WorldInfoWriter>,
    snapshots: impl IntoIterator<Item = (PathBuf, LevelData)> + Send + 'static,
    save: OwnedMutexGuard<()>,
) -> Result<OwnedMutexGuard<()>, WorldInfoError> {
    // Keep the guard in the writer even when its caller disconnects.
    tokio::task::spawn_blocking(move || {
        write_snapshots(writer.as_ref(), snapshots)?;
        Ok(save)
    })
    .await
    .map_err(io::Error::other)?
}

fn write_snapshots(
    writer: &dyn WorldInfoWriter,
    snapshots: impl IntoIterator<Item = (PathBuf, LevelData)>,
) -> Result<(), WorldInfoError> {
    let mut failure = None;
    for (path, info) in snapshots {
        if let Err(err) = writer.write_world_info(&info, &path) {
            error!(directory = %path.display(), %err, "Failed to save level metadata");
            failure.get_or_insert(err);
        }
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, mpsc};
    use std::time::Duration;

    struct HeldWriter {
        entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl WorldInfoWriter for HeldWriter {
        fn write_world_info(&self, _: &LevelData, _: &Path) -> Result<(), WorldInfoError> {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                let _ = entered.send(());
            }
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_cancelled_metadata_save_holds_the_writer_until_the_disk_write_ends() {
        let save = Arc::new(tokio::sync::Mutex::new(()));
        let (entered, started) = tokio::sync::oneshot::channel();
        let (resume, release) = mpsc::channel();
        let writer = Arc::new(HeldWriter {
            entered: Mutex::new(Some(entered)),
            release: Mutex::new(release),
        });
        let task = tokio::spawn(save_snapshots(
            writer,
            [(PathBuf::from("world"), LevelData::default(Seed(7)))],
            save.clone().lock_owned().await,
        ));
        started.await.unwrap();
        task.abort();
        assert!(task.await.is_err());
        assert!(save.try_lock().is_err());
        resume.send(()).unwrap();
        let _next = tokio::time::timeout(Duration::from_secs(3), save.lock())
            .await
            .unwrap();
    }
}
