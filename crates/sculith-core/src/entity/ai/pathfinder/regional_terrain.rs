use std::{
    collections::hash_map::Entry,
    sync::{Arc, Mutex, PoisonError},
};

use dashmap::try_result::TryResult;
use pumpkin_data::BlockStateId;
use rustc_hash::FxHashMap;
use sculith_util::math::{position::BlockPos, vector2::Vector2};
use sculith_world::level::SyncChunk;

use crate::world::{RegionChunks, RegionId, World};

const MAX_PATH_TERRAIN_CHUNKS: usize = 16;
type PaletteRevision = (SyncChunk, (u64, u64));

/// Records the terrain consulted by one attempt. A missing palette or a change in owner,
/// loaded chunk identity or revision leaves the existing navigator untouched.
pub(super) struct RegionalTerrain {
    owner: RegionId,
    extent: RegionChunks,
    read: Mutex<Option<FxHashMap<Vector2<i32>, PaletteRevision>>>,
}

impl RegionalTerrain {
    pub(super) fn new(owner: RegionId, extent: RegionChunks) -> Self {
        Self {
            owner,
            extent,
            read: Mutex::new(Some(FxHashMap::default())),
        }
    }

    fn owns(&self, world: &World, chunk: Vector2<i32>) -> bool {
        let layout = world.region_layout.load();
        layout
            .chunks(self.owner)
            .is_some_and(|current| Arc::ptr_eq(current, &self.extent))
            && layout.owner(chunk) == Some(self.owner)
    }

    pub(super) fn block_state(&self, world: &World, position: BlockPos) -> Option<BlockStateId> {
        if !world.is_in_build_limit(position) {
            return Some(BlockStateId::AIR);
        }
        let mut guard = self.read.lock().unwrap_or_else(PoisonError::into_inner);
        let read = guard.as_mut()?;
        let chunk_pos = position.chunk_position();
        let result = (|| {
            if !self.owns(world, chunk_pos)
                || (read.len() == MAX_PATH_TERRAIN_CHUNKS && !read.contains_key(&chunk_pos))
            {
                return None;
            }
            let (chunk, revision) = match read.entry(chunk_pos) {
                Entry::Occupied(recorded) => recorded.into_mut(),
                Entry::Vacant(recorded) => {
                    let TryResult::Present(loaded) = world.level.loaded_chunks.try_get(&chunk_pos)
                    else {
                        return None;
                    };
                    recorded.insert((loaded.value().clone(), loaded.section.terrain_revision()))
                }
            };
            world
                .level
                .try_with_block_state(position, |current, state| {
                    (std::ptr::eq(current, chunk.as_ref())
                        && *revision == current.section.terrain_revision())
                    .then_some(state)
                })?
        })();
        if result.is_none() {
            *guard = None;
        }
        result
    }

    /// The caller holds the actor assignment and region permits. Pin all the checked palettes
    /// together so a concurrent ordinary world write cannot interleave with the route commit.
    pub(super) fn try_commit<T>(&self, world: &World, commit: impl FnOnce() -> T) -> Option<T> {
        let guard = self.read.lock().unwrap_or_else(PoisonError::into_inner);
        let read = guard.as_ref()?;
        if read.is_empty() {
            return None;
        }
        pin_revisions(world, &mut read.values(), || {
            read.keys()
                .all(|&chunk| self.owns(world, chunk))
                .then(commit)
        })?
    }
}

fn pin_revisions<'a, T>(
    world: &World,
    revisions: &mut impl Iterator<Item = &'a PaletteRevision>,
    commit: impl FnOnce() -> T,
) -> Option<T> {
    let Some((chunk, revision)) = revisions.next() else {
        return Some(commit());
    };
    world
        .level
        .try_with_current_terrain(chunk, *revision, || pin_revisions(world, revisions, commit))?
}
