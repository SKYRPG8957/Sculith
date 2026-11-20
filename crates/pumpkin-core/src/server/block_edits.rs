//! Conditional block writes submitted to the simulation owner.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, PoisonError},
};

use pumpkin_data::{BlockDirection, BlockStateId};
use pumpkin_util::math::{position::BlockPos, vector2::Vector2};
use pumpkin_world::{lighting::LightEngine, world::BlockFlags};
use rustc_hash::FxHashSet;
use tokio::sync::oneshot;

use crate::world::World;

const BLOCK_EDIT_QUEUE_CAPACITY: usize = 1_024;
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

#[derive(Default)]
pub struct BlockEdits {
    pending: VecDeque<BlockEdit>,
    world_claimed: FxHashSet<Vector2<i32>>,
}

struct BlockEdit {
    position: BlockPos,
    expected: BlockStateId,
    replacement: BlockStateId,
    flags: BlockFlags,
    result: oneshot::Sender<BlockEditStatus>,
}

impl BlockEdit {
    fn apply(&self, world: &Arc<World>) -> BlockEditStatus {
        world
            .set_block_state_if(&self.position, self.replacement, self.flags, |current| {
                current == self.expected
            })
            .map_or(BlockEditStatus::Stale, |_| BlockEditStatus::Applied)
    }

    fn try_apply_in_region(
        &self,
        world: &World,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<BlockEditStatus> {
        if self
            .flags
            .intersects(BlockFlags::NOTIFY_NEIGHBORS | BlockFlags::MOVED | BlockFlags::FORCE_STATE)
            || LightEngine::has_different_light_properties(
                self.expected.to_state(),
                self.replacement.to_state(),
            )
            || [self.expected, self.replacement].into_iter().any(|state| {
                let block = state.to_block();
                let fluid = World::get_fluid_from_state_id(state);
                block.default_state.block_entity_type != u16::MAX
                    || world.block_registry.get_pumpkin_block(block.id).is_some()
                    || world.block_registry.get_pumpkin_fluid(fluid.id).is_some()
            })
        {
            return None;
        }

        let applied = world.level.try_apply_regional_block_edit(
            self.position,
            self.expected,
            self.replacement,
            owns,
            |read| {
                self.flags.contains(BlockFlags::UPDATE_KNOWN_SHAPE)
                    || BlockDirection::all().into_iter().all(|direction| {
                        let pos = self.position.offset(direction.to_offset());
                        world.is_in_build_limit(pos)
                            && read(pos).is_some_and(|neighbor| {
                                world
                                    .block_registry
                                    .get_pumpkin_block(neighbor.to_block_id())
                                    .is_none()
                            })
                    })
            },
        )?;
        if !applied {
            return Some(BlockEditStatus::Stale);
        }
        if self.expected != self.replacement && self.flags.contains(BlockFlags::NOTIFY_LISTENERS) {
            world.queue_regional_block_updates([(self.position, self.replacement)]);
        }
        Some(BlockEditStatus::Applied)
    }
}

impl World {
    /// Queues a conditional edit for a later simulation boundary. The owner applies it only
    /// while the same world and chunk are loaded and the expected state is still current.
    /// Returns `None` when the position is invalid, the queue is full, or admission is closed.
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
        let world_admission = self
            .player_admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !*world_admission {
            return None;
        }
        let mut queue = self
            .block_edits
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if queue.pending.len() >= BLOCK_EDIT_QUEUE_CAPACITY {
            return None;
        }
        let (result, receiver) = oneshot::channel();
        queue.pending.push_back(BlockEdit {
            position,
            expected,
            replacement,
            flags,
            result,
        });
        Some(BlockEditReceipt {
            result: receiver,
            status: BlockEditStatus::Pending,
        })
    }
    /// The world owns callbacks and any edits not yet picked up by a region.
    pub(crate) fn apply_block_edits(self: &Arc<Self>) {
        let Some(queue) = self.block_edits.get() else {
            return;
        };
        let batch = {
            let mut edits = queue.lock().unwrap_or_else(PoisonError::into_inner);
            let count = edits.pending.len().min(BLOCK_EDITS_PER_TICK);
            if count == 0 {
                return;
            }
            let batch: Vec<_> = edits.pending.drain(..count).collect();
            edits
                .world_claimed
                .extend(batch.iter().map(|edit| edit.position.chunk_position()));
            batch
        };
        for edit in batch {
            let status = edit.apply(self);
            let _ = edit.result.send(status);
        }
        queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .world_claimed
            .clear();
    }

    pub(crate) fn apply_regional_block_edits(&self, owns: &impl Fn(Vector2<i32>) -> bool) {
        let Some(queue) = self.block_edits.get() else {
            return;
        };
        let Ok(mut edits) = queue.try_lock() else {
            return;
        };
        if edits.world_claimed.iter().copied().any(owns) {
            return;
        }
        let mut index = 0;
        let mut applied = 0;
        while applied < BLOCK_EDITS_PER_TICK {
            let Some(edit) = edits.pending.get(index) else {
                break;
            };
            if !owns(edit.position.chunk_position()) {
                index += 1;
                continue;
            }
            // A deferred edit still runs before later requests in its region.
            let Some(status) = edit.try_apply_in_region(self, owns) else {
                break;
            };
            if let Some(edit) = edits.pending.remove(index) {
                let _ = edit.result.send(status);
            }
            applied += 1;
        }
    }

    pub(crate) fn cancel_block_edits(&self) {
        if let Some(queue) = self.block_edits.get() {
            queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pending
                .clear();
        }
    }
}
