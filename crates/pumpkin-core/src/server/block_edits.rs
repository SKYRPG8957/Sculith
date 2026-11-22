//! Conditional block writes submitted to the simulation owner.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, PoisonError, Weak},
};

use pumpkin_data::{BlockDirection, BlockStateId};
use pumpkin_util::math::{position::BlockPos, vector2::Vector2, vector3::Vector3};
use pumpkin_world::{
    chunk::{CHUNK_WIDTH, ChunkData},
    level::SyncChunk,
    lighting::LightEngine,
    world::BlockFlags,
};
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
    claimed_chunks: FxHashSet<Vector2<i32>>,
}

struct BlockEdit {
    request: BlockEditRequest,
    result: oneshot::Sender<BlockEditStatus>,
}

#[derive(Clone)]
struct BlockEditRequest {
    position: BlockPos,
    chunk: Weak<ChunkData>,
    expected: BlockStateId,
    replacement: BlockStateId,
    flags: BlockFlags,
}

impl BlockEditRequest {
    fn apply(&self, world: &Arc<World>) -> BlockEditStatus {
        let Some(chunk) = self.chunk.upgrade() else {
            return BlockEditStatus::Stale;
        };
        let Some(previous) =
            world.write_block_state_if(&self.position, self.replacement, |live, state| {
                Arc::ptr_eq(&chunk, live) && state == self.expected
            })
        else {
            return BlockEditStatus::Stale;
        };
        world.on_block_state_set(&self.position, previous, self.replacement, self.flags);
        BlockEditStatus::Applied
    }

    fn try_apply_in_region(
        &self,
        world: &World,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<BlockEditStatus> {
        let Some(chunk) = self.chunk.upgrade() else {
            return Some(BlockEditStatus::Stale);
        };
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
            &chunk,
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

impl BlockEdits {
    fn claim_world(&mut self) -> Vec<BlockEdit> {
        // A regional claim leaves its place in the FIFO until the commit is reported.
        let count = self
            .pending
            .iter()
            .take(BLOCK_EDITS_PER_TICK)
            .take_while(|edit| {
                !self
                    .claimed_chunks
                    .contains(&edit.request.position.chunk_position())
            })
            .count();
        let batch: Vec<_> = self.pending.drain(..count).collect();
        self.claimed_chunks.extend(
            batch
                .iter()
                .map(|edit| edit.request.position.chunk_position()),
        );
        batch
    }

    fn claim_region(&mut self, belongs: &impl Fn(Vector2<i32>) -> bool) -> Vec<BlockEditRequest> {
        if self.claimed_chunks.iter().copied().any(belongs) {
            return Vec::new();
        }
        let requests: Vec<_> = self
            .pending
            .iter()
            .filter(|edit| belongs(edit.request.position.chunk_position()))
            .take(BLOCK_EDITS_PER_TICK)
            .map(|edit| edit.request.clone())
            .collect();
        self.claimed_chunks
            .extend(requests.iter().map(|edit| edit.position.chunk_position()));
        requests
    }

    fn finish_region(
        &mut self,
        requests: &[BlockEditRequest],
        results: impl IntoIterator<Item = BlockEditStatus>,
    ) {
        // Each position's oldest entry belongs to this batch until its claim is released.
        // Completion uses that original position even if the region layout already changed.
        for (request, status) in requests.iter().zip(results) {
            if let Some(index) = self
                .pending
                .iter()
                .position(|edit| edit.request.position == request.position)
                && let Some(edit) = self.pending.remove(index)
            {
                let _ = edit.result.send(status);
            }
        }
        self.release(requests.iter().map(|edit| edit.position.chunk_position()));
    }

    fn release(&mut self, chunks: impl IntoIterator<Item = Vector2<i32>>) {
        for chunk in chunks {
            self.claimed_chunks.remove(&chunk);
        }
    }
}

fn run_regional_edits(
    queue: &Mutex<BlockEdits>,
    belongs: &impl Fn(Vector2<i32>) -> bool,
    apply: impl FnMut(&BlockEditRequest) -> Option<BlockEditStatus>,
) {
    let requests = {
        let Ok(mut edits) = queue.try_lock() else {
            return;
        };
        edits.claim_region(belongs)
    };
    if requests.is_empty() {
        return;
    }

    // Keep a deferred request and its successors in the same order for the next owner.
    let completed: Vec<_> = requests.iter().map_while(apply).collect();
    queue
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .finish_region(&requests, completed);
}

impl World {
    fn with_block_edit_admission<R>(&self, write: impl FnOnce() -> R) -> Option<R> {
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
        (*world_admission).then(write)
    }

    /// Writes a local block through an existing chunk handle without running block callbacks.
    /// The loaded map pins the same instance for the duration of the write.
    pub fn set_loaded_chunk_block_state(
        &self,
        chunk: &SyncChunk,
        relative: Vector3<i32>,
        state: BlockStateId,
    ) -> Result<(), &'static str> {
        const WIDTH: i32 = CHUNK_WIDTH as i32;
        if !(0..WIDTH).contains(&relative.x) || !(0..WIDTH).contains(&relative.z) {
            return Err("Block position is outside the chunk");
        }
        let x = chunk.x.checked_mul(WIDTH).ok_or("Invalid chunk origin")? + relative.x;
        let z = chunk.z.checked_mul(WIDTH).ok_or("Invalid chunk origin")? + relative.z;
        let position = BlockPos::new(x, relative.y, z);
        if !self.is_in_build_limit(position) {
            return Err("Block position is outside the world build limit");
        }
        self.with_block_edit_admission(|| {
            let previous = self
                .write_block_state_if(&position, state, |live, _| Arc::ptr_eq(chunk, live))
                .ok_or("Chunk unloaded")?;
            if previous != state {
                self.register_block_change(position, state);
            }
            Ok(())
        })
        .ok_or("World unavailable")?
    }

    /// Queues a conditional edit for a later simulation boundary. The owner applies it only
    /// while the chunk loaded at submission is current and the expected state still matches.
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
        let request = BlockEditRequest {
            position,
            chunk: self
                .level
                .read_chunk_sync(&position.chunk_position(), Arc::downgrade)
                .unwrap_or_default(),
            expected,
            replacement,
            flags,
        };
        self.with_block_edit_admission(|| {
            let mut queue = self
                .block_edits
                .get_or_init(Mutex::default)
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if queue.pending.len() >= BLOCK_EDIT_QUEUE_CAPACITY {
                return None;
            }
            let (result, receiver) = oneshot::channel();
            queue.pending.push_back(BlockEdit { request, result });
            Some(BlockEditReceipt {
                result: receiver,
                status: BlockEditStatus::Pending,
            })
        })?
    }
    /// The world owns callbacks and any edits not yet picked up by a region.
    pub(crate) fn apply_block_edits(self: &Arc<Self>) {
        let Some(queue) = self.block_edits.get() else {
            return;
        };
        let batch = {
            let mut edits = queue.lock().unwrap_or_else(PoisonError::into_inner);
            edits.claim_world()
        };
        if batch.is_empty() {
            return;
        }
        let mut chunks = Vec::with_capacity(batch.len());
        for edit in batch {
            chunks.push(edit.request.position.chunk_position());
            let status = edit.request.apply(self);
            let _ = edit.result.send(status);
        }
        queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .release(chunks);
    }

    pub(crate) fn apply_regional_block_edits(
        &self,
        belongs: &impl Fn(Vector2<i32>) -> bool,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) {
        let Some(queue) = self.block_edits.get() else {
            return;
        };
        run_regional_edits(queue, belongs, |edit| edit.try_apply_in_region(self, owns));
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

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, thread, time::Duration};

    use super::*;

    fn submit(edits: &mut BlockEdits, x: i32) -> oneshot::Receiver<BlockEditStatus> {
        let (result, receipt) = oneshot::channel();
        edits.pending.push_back(BlockEdit {
            request: BlockEditRequest {
                position: BlockPos::new(x, 90, 4),
                chunk: Weak::new(),
                expected: BlockStateId::AIR,
                replacement: BlockStateId::AIR,
                flags: BlockFlags::NOTIFY_LISTENERS,
            },
            result,
        });
        receipt
    }

    #[test]
    fn region_claim_leaves_the_queue_available_and_defers_in_place() {
        let mut edits = BlockEdits::default();
        let mut first = submit(&mut edits, 1);
        let mut east = submit(&mut edits, 160);
        let mut deferred = submit(&mut edits, 17);
        let queue = Mutex::new(edits);
        let (begun, wait_for_claim) = mpsc::channel();
        let (resume, wait_for_resume) = mpsc::channel();
        let timeout = Duration::from_secs(5);

        let mut late = thread::scope(|scope| {
            let shared_queue = &queue;
            scope.spawn(move || {
                run_regional_edits(shared_queue, &|chunk| chunk.x < 2, |edit| {
                    (edit.position == BlockPos::new(1, 90, 4)).then(|| {
                        begun.send(()).unwrap();
                        wait_for_resume.recv_timeout(timeout).unwrap();
                        BlockEditStatus::Applied
                    })
                });
            });
            wait_for_claim.recv_timeout(timeout).unwrap();

            let late = {
                let mut edits = queue.try_lock().unwrap();
                assert!(edits.claim_world().is_empty());
                submit(&mut edits, 1)
            };
            run_regional_edits(&queue, &|chunk| chunk.x == 10, |_| {
                Some(BlockEditStatus::Applied)
            });
            assert_eq!(east.try_recv(), Ok(BlockEditStatus::Applied));
            assert_eq!(first.try_recv(), Err(oneshot::error::TryRecvError::Empty));
            resume.send(()).unwrap();
            late
        });

        assert_eq!(first.try_recv(), Ok(BlockEditStatus::Applied));
        assert_eq!(
            deferred.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        );
        assert_eq!(late.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        let batch = queue.into_inner().unwrap().claim_world();
        assert_eq!(
            batch
                .iter()
                .map(|edit| edit.request.position)
                .collect::<Vec<_>>(),
            [BlockPos::new(17, 90, 4), BlockPos::new(1, 90, 4)],
        );
    }
}
