use std::collections::hash_map::Entry;

use pumpkin_util::math::vector2::Vector2;
use rustc_hash::{FxHashMap, FxHashSet};

/// Chunk ownership for collecting pending block and fluid ticks.
#[derive(Default)]
pub(super) struct TickRegions {
    owners: FxHashMap<Vector2<i32>, usize>,
    count: usize,
    needs_rebuild: bool,
}

impl TickRegions {
    pub(super) const fn active_chunks_changed(&mut self) {
        self.needs_rebuild = true;
    }

    pub(super) fn group_scheduled(
        &mut self,
        active: &FxHashSet<Vector2<i32>>,
        scheduled: Vec<Vector2<i32>>,
    ) -> Vec<Vec<Vector2<i32>>> {
        if std::mem::take(&mut self.needs_rebuild) {
            self.rebuild(active);
        }
        if self.count < 2 {
            return vec![scheduled];
        }

        let mut groups: FxHashMap<usize, Vec<_>> = FxHashMap::default();
        for chunk in scheduled {
            // Loaded chunks without an active owner still have advancing timers.
            let owner = self.owners.get(&chunk).copied().unwrap_or(self.count);
            groups.entry(owner).or_default().push(chunk);
        }
        groups.into_values().collect()
    }

    fn rebuild(&mut self, active: &FxHashSet<Vector2<i32>>) {
        let previous = self.count;
        self.owners.clear();
        self.owners.reserve(active.len());
        self.count = 0;
        let mut visit = Vec::new();
        for &start in active {
            let Entry::Vacant(start_owner) = self.owners.entry(start) else {
                continue;
            };
            start_owner.insert(self.count);
            visit.push(start);
            while let Some(chunk) = visit.pop() {
                for x in -1..=1 {
                    for z in -1..=1 {
                        let neighbor = chunk.add_raw(x, z);
                        if active.contains(&neighbor)
                            && let Entry::Vacant(owner) = self.owners.entry(neighbor)
                        {
                            owner.insert(self.count);
                            visit.push(neighbor);
                        }
                    }
                }
            }
            self.count += 1;
        }
        if previous != self.count {
            tracing::trace!(
                previous,
                regions = self.count,
                active_chunks = active.len(),
                "Scheduled chunk regions changed"
            );
        }
    }
}
