use std::{
    io,
    path::{Path, PathBuf},
    sync::{Condvar, Mutex, PoisonError},
};

use rustc_hash::FxHashSet;
use tokio::sync::Notify;

#[derive(Default)]
pub(super) struct WorldOpenings {
    state: Mutex<State>,
    changed: Condvar,
    drained: Notify,
}

#[derive(Default)]
struct State {
    opening: FxHashSet<PathBuf>,
    closed: bool,
}

pub(super) struct Opening<'a> {
    registry: &'a WorldOpenings,
    location: &'a Path,
}

impl WorldOpenings {
    pub(super) fn reserve<'a>(&'a self, location: &'a Path) -> io::Result<Opening<'a>> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut state = self
            .changed
            .wait_while(state, |state| {
                !state.closed && state.opening.contains(location)
            })
            .unwrap_or_else(PoisonError::into_inner);
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Server is stopping",
            ));
        }
        state.opening.insert(location.to_path_buf());
        Ok(Opening {
            registry: self,
            location,
        })
    }

    pub(super) async fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closed = true;
        self.changed.notify_all();
        loop {
            let finished = self.drained.notified();
            tokio::pin!(finished);
            finished.as_mut().enable();
            if self
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .opening
                .is_empty()
            {
                return;
            }
            finished.await;
        }
    }
}

impl Drop for Opening<'_> {
    fn drop(&mut self) {
        let mut state = self
            .registry
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.opening.remove(self.location);
        self.registry.changed.notify_all();
        if state.opening.is_empty() {
            self.registry.drained.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{data::VanillaData, server::Server};
    use pumpkin_data::dimension::Dimension;
    use sculith_config::{AdvancedConfiguration, BasicConfiguration, TelemetryConfig};
    use std::sync::{Arc, Barrier};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_openings_share_a_canonical_world_owner() {
        let folder = tempfile::tempdir().unwrap();
        let basic = BasicConfiguration {
            default_level_name: folder.path().join("world").to_string_lossy().into_owned(),
            allow_nether: false,
            allow_end: false,
            ..Default::default()
        };
        let mut advanced = AdvancedConfiguration::default();
        advanced.world.generator = sculith_config::world::WorldPreset::Flat;
        let server = Server::new(
            basic,
            advanced,
            TelemetryConfig::default(),
            VanillaData::default(),
            Vec::new(),
        )
        .await
        .unwrap();

        let requests = Barrier::new(4);
        let [world, alias, trailing_slash, nether] = std::thread::scope(|threads| {
            let handles = [
                ("copy", Dimension::OVERWORLD),
                ("./copy", Dimension::OVERWORLD),
                ("copy/", Dimension::OVERWORLD),
                ("./copy", Dimension::THE_NETHER),
            ]
            .map(|(name, dimension)| {
                let requests = &requests;
                let server = &server;
                threads.spawn(move || {
                    let _runtime = server.runtime.enter();
                    requests.wait();
                    server.try_create_world(name.into(), dimension).unwrap()
                })
            });
            handles.map(|handle| handle.join().unwrap())
        });
        let overworld = server.worlds.load().first().unwrap().clone();
        assert!(
            server
                .find_sibling_dimension(&overworld, &Dimension::THE_NETHER)
                .is_none()
        );
        assert!(Arc::ptr_eq(
            &server
                .find_sibling_dimension(&world, &Dimension::THE_NETHER)
                .unwrap(),
            &nether,
        ));
        assert!(Arc::ptr_eq(
            &server
                .find_sibling_dimension(&nether, &Dimension::OVERWORLD)
                .unwrap(),
            &world,
        ));
        let primary_count = server.worlds.load().len();
        server.shutdown().await;

        assert!(Arc::ptr_eq(&world, &alias));
        assert!(Arc::ptr_eq(&world, &trailing_slash));
        assert!(!Arc::ptr_eq(&world, &nether));
        assert!(Arc::ptr_eq(&world.level_info, &nether.level_info));
        assert!(!Arc::ptr_eq(&world.level_info, &server.level_info));
        assert_eq!(primary_count, 3);
        let stopped = server.try_create_world("copy".into(), Dimension::OVERWORLD);
        assert!(
            matches!(stopped, Err(sculith_world::world_info::WorldInfoError::IoError(error))
            if error.kind() == io::ErrorKind::Interrupted)
        );
    }

    #[tokio::test]
    async fn shutdown_waits_for_a_world_opening_without_blocking_other_directories() {
        let registry = WorldOpenings::default();
        let a = PathBuf::from("first");
        let b = PathBuf::from("second");
        let first = registry.reserve(&a).unwrap();
        let second = registry.reserve(&b).unwrap();
        let shutdown = registry.close();
        tokio::pin!(shutdown);
        assert!(futures::poll!(shutdown.as_mut()).is_pending());
        assert!(registry.reserve(&b).is_err());
        drop(first);
        assert!(futures::poll!(shutdown.as_mut()).is_pending());
        drop(second);
        shutdown.await;
    }
}
