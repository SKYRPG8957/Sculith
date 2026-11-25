use crate::net::chunk_sender::PreparedChunk;
use crate::server::Server;
use crate::world::World;
use pumpkin_macros::{Event, cancellable};
use pumpkin_world::chunk::ChunkData;
use pumpkin_world::level::SyncChunk;
use sculith_util::math::vector2::Vector2;
use std::sync::Arc;

/// An event that occurs when a chunk is sent to a client.
///
/// This event contains information about the world and the chunk being sent.
/// Cancelling holds the chunk back -> it is offered again about a second later.
#[cancellable]
#[derive(Event, Clone)]
pub struct ChunkSend {
    /// The world from which the chunk is being sent.
    pub world: Arc<World>,

    /// The chunk data being sent.
    pub chunk: Arc<ChunkData>,
}

impl ChunkSend {
    pub const fn new(world: Arc<World>, chunk: Arc<ChunkData>) -> Self {
        Self {
            world,
            chunk,
            cancelled: false,
        }
    }

    /// Fires the event per chunk -> returns the chunks no plugin cancelled.
    pub async fn filter(
        server: &Arc<Server>,
        world: &Arc<World>,
        chunks: &[SyncChunk],
    ) -> Vec<SyncChunk> {
        Self::filter_while(server, world, chunks, || true)
            .await
            .unwrap_or_default()
    }

    pub(crate) async fn filter_while(
        server: &Arc<Server>,
        world: &Arc<World>,
        chunks: &[SyncChunk],
        current: impl Fn() -> bool,
    ) -> Option<Vec<SyncChunk>> {
        let mut valid_chunks = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            if !current() {
                return None;
            }
            let mut event = Self::new(world.clone(), chunk.clone());
            server.plugin_manager.fire(server, &mut event).await;
            if !event.cancelled {
                valid_chunks.push(chunk.clone());
            }
        }
        current().then_some(valid_chunks)
    }

    /// Blocking per-chunk fire for the tick -> removes cancelled chunks, returns their positions.
    pub fn retain_blocking(
        server: &Arc<Server>,
        world: &Arc<World>,
        chunks: &mut Vec<PreparedChunk>,
    ) -> Vec<Vector2<i32>> {
        Self::retain_blocking_while(server, world, chunks, || true).unwrap_or_default()
    }

    pub(crate) fn retain_blocking_while(
        server: &Arc<Server>,
        world: &Arc<World>,
        chunks: &mut Vec<PreparedChunk>,
        current: impl Fn() -> bool,
    ) -> Option<Vec<Vector2<i32>>> {
        let mut cancelled = Vec::new();
        chunks.retain(|prepared| {
            if !current() {
                return true;
            }
            let mut event = Self::new(world.clone(), prepared.chunk.clone());
            server.plugin_manager.fire_blocking(server, &mut event);
            if event.cancelled {
                cancelled.push(prepared.position);
            }
            !event.cancelled
        });
        current().then_some(cancelled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        data::VanillaData,
        plugin::{EventHandler, api::events::EventPriority},
    };
    use futures::future::BoxFuture;
    use sculith_config::{AdvancedConfiguration, BasicConfiguration, telemetry::TelemetryConfig};
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    #[derive(Default)]
    struct ViewChange {
        called: AtomicUsize,
        epoch: AtomicUsize,
    }

    impl EventHandler<ChunkSend> for ViewChange {
        fn handle_blocking<'a>(
            &'a self,
            _: &'a Arc<Server>,
            event: &'a mut ChunkSend,
        ) -> BoxFuture<'a, ()> {
            Box::pin(async {
                self.called.fetch_add(1, Relaxed);
                if event.chunk.x == 0 {
                    self.epoch.fetch_add(1, Relaxed);
                }
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terrain_hooks_stop_after_a_view_changes_between_columns() {
        let folder = tempfile::tempdir().unwrap();
        let server = Server::new(
            BasicConfiguration {
                default_level_name: folder.path().join("world").to_string_lossy().into_owned(),
                allow_nether: false,
                allow_end: false,
                ..Default::default()
            },
            AdvancedConfiguration::default(),
            TelemetryConfig::default(),
            VanillaData::default(),
            Vec::new(),
        )
        .await
        .unwrap();
        let world = server.worlds.load()[0].clone();
        let observer = Arc::new(ViewChange::default());
        server
            .plugin_manager
            .register(observer.clone(), EventPriority::Normal, true);
        let chunks = [ChunkData::empty_sync(0, 0), ChunkData::empty_sync(1, 0)];
        assert!(
            ChunkSend::filter_while(&server, &world, &chunks, || {
                observer.epoch.load(Relaxed) == 0
            })
            .await
            .is_none()
        );
        assert_eq!(observer.called.load(Relaxed), 1);

        let mut prepared = chunks
            .iter()
            .map(|chunk| PreparedChunk {
                position: Vector2::new(chunk.x, chunk.z),
                chunk: chunk.clone(),
            })
            .collect();
        assert!(
            ChunkSend::retain_blocking_while(&server, &world, &mut prepared, || {
                observer.epoch.load(Relaxed) == 1
            })
            .is_none()
        );
        assert_eq!(observer.called.load(Relaxed), 2);

        let ready = ChunkSend::filter_while(&server, &world, &chunks[1..], || {
            observer.epoch.load(Relaxed) == 2
        })
        .await
        .unwrap();
        assert_eq!(ready.len(), 1);
        assert!(Arc::ptr_eq(&ready[0], &chunks[1]));
        assert_eq!(observer.called.load(Relaxed), 3);
    }
}
