//! One logical world owner. Protocol tasks receive immutable snapshots only.

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

#[cfg(test)]
use anyhow::ensure;
use anyhow::{Context, Result};
#[cfg(test)]
use sculith_world::{BlockEdit, CommittedBatch, MAX_EDITS_PER_BATCH};
use sculith_world::{ChunkPos, ChunkSnapshot, Region};
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Clone)]
pub struct WorldHandle {
    commands: mpsc::Sender<Command>,
    online: Arc<AtomicU32>,
}

enum Command {
    Snapshot {
        position: ChunkPos,
        reply: oneshot::Sender<sculith_world::Result<ChunkSnapshot>>,
    },
    EntityId {
        reply: oneshot::Sender<Option<u64>>,
    },
    #[cfg(test)]
    Commit {
        revision: u64,
        edits: Vec<BlockEdit>,
        reply: oneshot::Sender<sculith_world::Result<CommittedBatch>>,
    },
}

/// Counts initialized sessions, not unverified RakNet connections.
pub struct PlayerLease {
    online: Arc<AtomicU32>,
}

impl Drop for PlayerLease {
    fn drop(&mut self) {
        self.online.fetch_sub(1, Ordering::AcqRel);
    }
}

impl WorldHandle {
    pub async fn snapshot(&self, position: ChunkPos) -> Result<ChunkSnapshot> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::Snapshot { position, reply })
            .await
            .context("world owner stopped")?;
        Ok(receiver
            .await
            .context("world owner dropped snapshot request")??)
    }

    pub async fn entity_id(&self) -> Result<u64> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::EntityId { reply })
            .await
            .context("world owner stopped")?;
        receiver
            .await
            .context("world owner dropped entity ID request")?
            .context("world entity IDs exhausted")
    }

    pub fn online_players(&self) -> u32 {
        self.online.load(Ordering::Acquire)
    }

    pub fn initialized_player(&self) -> PlayerLease {
        self.online.fetch_add(1, Ordering::AcqRel);
        PlayerLease {
            online: self.online.clone(),
        }
    }

    #[cfg(test)]
    async fn commit(&self, revision: u64, edits: Vec<BlockEdit>) -> Result<CommittedBatch> {
        ensure!(edits.len() <= MAX_EDITS_PER_BATCH, "too many world edits");
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::Commit {
                revision,
                edits,
                reply,
            })
            .await
            .context("world owner stopped")?;
        Ok(receiver.await.context("world owner dropped commit")??)
    }
}

pub fn create(
    mut shutdown: watch::Receiver<bool>,
) -> (WorldHandle, impl Future<Output = Result<()>> + Send) {
    let (commands, mut requests) = mpsc::channel(32);
    let handle = WorldHandle {
        commands,
        online: Arc::new(AtomicU32::new(0)),
    };
    let task = async move {
        let mut region = Region::flat_origin();
        let mut next_entity = 1u64;
        loop {
            tokio::select! {
                stopped = shutdown.changed() => {
                    if stopped.is_err() || *shutdown.borrow() { return Ok(()); }
                }
                command = requests.recv() => {
                    match command {
                        Some(Command::Snapshot { position, reply }) => { let _ = reply.send(region.snapshot(position)); }
                        Some(Command::EntityId { reply }) => {
                            let next = next_entity.checked_add(1);
                            let _ = reply.send(next.map(|_| next_entity));
                            if let Some(value) = next { next_entity = value; }
                        }
                        #[cfg(test)]
                        Some(Command::Commit { revision, edits, reply }) => { let _ = reply.send(region.commit(revision, &edits)); }
                        None => return Ok(()),
                    }
                }
            }
        }
    };
    (handle, task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sculith_world::{BlockKind, BlockPos};

    #[tokio::test]
    async fn concurrent_protocol_handles_share_one_owner_and_old_snapshots_stay_immutable() {
        let (shutdown, stop) = watch::channel(false);
        let (world, task) = create(stop);
        let task = tokio::spawn(task);
        let old = world.snapshot(ChunkPos { x: 0, z: 0 }).await.unwrap();
        let position = BlockPos { x: 1, y: 4, z: 2 };
        let (first, second) = tokio::join!(
            world.commit(
                0,
                vec![BlockEdit {
                    position,
                    block: BlockKind::Stone
                }]
            ),
            world.commit(
                0,
                vec![BlockEdit {
                    position,
                    block: BlockKind::Dirt
                }]
            ),
        );
        assert_ne!(
            first.is_ok(),
            second.is_ok(),
            "only one revision-zero commit may succeed"
        );
        assert_eq!(old.get(1, 4, 2).unwrap(), BlockKind::Air);
        let current = world.snapshot(ChunkPos { x: 0, z: 0 }).await.unwrap();
        assert_eq!(current.revision(), 1);
        assert_ne!(current.get(1, 4, 2).unwrap(), BlockKind::Air);
        let (id1, id2) = tokio::join!(world.entity_id(), world.entity_id());
        assert_ne!(id1.unwrap(), id2.unwrap());
        shutdown.send_replace(true);
        task.await.unwrap().unwrap();
        assert!(world.snapshot(ChunkPos { x: 0, z: 0 }).await.is_err());
    }

    #[test]
    fn session_leases_count_initialized_players_and_cleanup_on_drop() {
        let (_shutdown, stop) = watch::channel(false);
        let (world, _task) = create(stop);
        let a = world.initialized_player();
        let b = world.initialized_player();
        assert_eq!(world.online_players(), 2);
        drop(a);
        assert_eq!(world.online_players(), 1);
        drop(b);
        assert_eq!(world.online_players(), 0);
    }
}
