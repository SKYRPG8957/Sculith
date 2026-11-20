//! Conditional block writes submitted to the server's simulation boundary.

use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use pumpkin_data::BlockStateId;
use pumpkin_util::math::position::BlockPos;
use pumpkin_world::world::BlockFlags;
use tokio::sync::oneshot;

use crate::world::World;

use super::Server;

pub(super) const BLOCK_EDIT_QUEUE_CAPACITY: usize = 1_024;
const BLOCK_EDITS_PER_TICK: usize = 32;

/// The simulation owner's response to a submitted edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockEditStatus {
    Pending,
    Applied,
    /// The expected state changed, or the owning chunk was unloaded.
    Stale,
    /// The world was unloaded or the server stopped.
    Unavailable,
}

/// A receipt that can be polled from a plugin tick hook without blocking simulation.
pub struct BlockEditReceipt {
    result: oneshot::Receiver<BlockEditStatus>,
    status: BlockEditStatus,
}

impl BlockEditReceipt {
    /// Returns the latest status. Completed results remain available on later calls.
    pub fn status(&mut self) -> BlockEditStatus {
        if self.status == BlockEditStatus::Pending {
            self.status = match self.result.try_recv() {
                Ok(status) => status,
                Err(oneshot::error::TryRecvError::Empty) => BlockEditStatus::Pending,
                Err(oneshot::error::TryRecvError::Closed) => BlockEditStatus::Unavailable,
            };
        }
        self.status
    }
}

pub(super) struct BlockEdit {
    world: Arc<World>,
    position: BlockPos,
    expected: BlockStateId,
    replacement: BlockStateId,
    flags: BlockFlags,
    result: oneshot::Sender<BlockEditStatus>,
}

impl BlockEdit {
    fn apply(&self, server: &Server) -> BlockEditStatus {
        if server.stop_token.is_cancelled()
            || !server
                .worlds
                .load()
                .iter()
                .any(|world| Arc::ptr_eq(world, &self.world))
        {
            return BlockEditStatus::Unavailable;
        }
        let _operation = {
            let admission = self
                .world
                .player_admission
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if !*admission {
                return BlockEditStatus::Unavailable;
            }
            self.world.pending_operations.fetch_add(1, Ordering::AcqRel);
            WorldOperation(&self.world)
        };
        self.world
            .set_block_state_if(&self.position, self.replacement, self.flags, |current| {
                current == self.expected
            })
            .map_or(BlockEditStatus::Stale, |_| BlockEditStatus::Applied)
    }
}

struct WorldOperation<'a>(&'a World);

impl Drop for WorldOperation<'_> {
    fn drop(&mut self) {
        self.0.pending_operations.fetch_sub(1, Ordering::Release);
    }
}

impl World {
    /// Queues a conditional edit for a later simulation boundary. The owner applies it only
    /// while the same world and chunk are loaded and the expected state is still current.
    /// Returns `None` when the position is invalid, the queue is full, or the server is stopping.
    /// Dropping the receipt leaves an accepted edit queued.
    pub fn schedule_block_edit(
        self: &Arc<Self>,
        position: BlockPos,
        expected: BlockStateId,
        replacement: BlockStateId,
        flags: BlockFlags,
    ) -> Option<BlockEditReceipt> {
        if !self.is_in_build_limit(position) {
            return None;
        }
        let server = self.server.upgrade()?;
        let closed = server
            .admission_closed
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if *closed || server.stop_token.is_cancelled() {
            return None;
        }
        let (result, receiver) = oneshot::channel();
        server
            .block_edits
            .push(BlockEdit {
                world: self.clone(),
                position,
                expected,
                replacement,
                flags,
                result,
            })
            .ok()?;
        Some(BlockEditReceipt {
            result: receiver,
            status: BlockEditStatus::Pending,
        })
    }
}

impl Server {
    pub(super) fn apply_block_edits(&self) {
        for _ in 0..self.block_edits.len().min(BLOCK_EDITS_PER_TICK) {
            let Some(edit) = self.block_edits.pop() else {
                break;
            };
            let status = edit.apply(self);
            let _ = edit.result.send(status);
        }
    }
}
