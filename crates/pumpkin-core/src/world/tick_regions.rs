use std::{cmp::Reverse, collections::hash_map::Entry};

use pumpkin_util::math::vector2::Vector2;
use pumpkin_world::level::ScheduledTickGroup;
use rustc_hash::{FxHashMap, FxHashSet};

/// Chunk ownership for collecting pending block and fluid ticks.
#[derive(Default)]
pub(super) struct TickRegions {
    owners: FxHashMap<Vector2<i32>, usize>,
    next_ticks: FxHashMap<usize, u64>,
    next_owner: usize,
    needs_rebuild: bool,
}

#[derive(Default)]
struct Component {
    anchor: (i32, i32),
    overlaps: FxHashMap<usize, usize>,
    next_tick: u64,
    owner: Option<usize>,
}

impl TickRegions {
    pub(super) const fn active_chunks_changed(&mut self) {
        self.needs_rebuild = true;
    }

    pub(super) fn step_scheduled(
        &mut self,
        active: &FxHashSet<Vector2<i32>>,
        scheduled: Vec<Vector2<i32>>,
    ) -> Vec<ScheduledTickGroup> {
        if std::mem::take(&mut self.needs_rebuild) {
            self.rebuild(active);
        }

        let mut groups: FxHashMap<Option<usize>, Vec<_>> = FxHashMap::default();
        for chunk in scheduled {
            // Loaded chunks without an active owner still have advancing timers.
            let owner = self.owners.get(&chunk).copied();
            groups.entry(owner).or_default().push(chunk);
        }
        let plan = groups
            .into_iter()
            .map(|(owner, chunks)| ScheduledTickGroup {
                chunks,
                region_tick: owner.map(|owner| self.next_ticks[&owner]),
            })
            .collect();
        for tick in self.next_ticks.values_mut() {
            *tick += 1;
        }
        plan
    }

    fn rebuild(&mut self, active: &FxHashSet<Vector2<i32>>) {
        let previous = self.next_ticks.len();
        let mut owners = FxHashMap::default();
        owners.reserve(active.len());
        let mut components = Vec::new();
        let mut visit = Vec::new();
        for &start in active {
            let Entry::Vacant(start_owner) = owners.entry(start) else {
                continue;
            };
            let index = components.len();
            start_owner.insert(index);
            let mut component = Component {
                anchor: (start.x, start.y),
                ..Component::default()
            };
            visit.push(start);
            while let Some(chunk) = visit.pop() {
                component.anchor = component.anchor.min((chunk.x, chunk.y));
                if let Some(&owner) = self.owners.get(&chunk) {
                    *component.overlaps.entry(owner).or_default() += 1;
                    component.next_tick = component.next_tick.max(self.next_ticks[&owner]);
                }
                for x in -1..=1 {
                    for z in -1..=1 {
                        let neighbor = chunk.add_raw(x, z);
                        if active.contains(&neighbor)
                            && let Entry::Vacant(owner) = owners.entry(neighbor)
                        {
                            owner.insert(index);
                            visit.push(neighbor);
                        }
                    }
                }
            }
            components.push(component);
        }

        // The component with the largest overlap claims an old owner. Ties use
        // the old owner and then the component's first position.
        let mut claims = Vec::new();
        for (index, component) in components.iter().enumerate() {
            for (&owner, &count) in &component.overlaps {
                claims.push((Reverse(count), owner, component.anchor, index));
            }
        }
        claims.sort_unstable();
        let mut retained = FxHashSet::default();
        for (_, owner, _, index) in claims {
            let component = &mut components[index];
            if component.owner.is_none() && retained.insert(owner) {
                component.owner = Some(owner);
            }
        }

        self.next_ticks.clear();
        let component_owners: Vec<_> = components
            .into_iter()
            .map(|component| {
                let owner = component.owner.unwrap_or_else(|| {
                    let owner = self.next_owner;
                    self.next_owner += 1;
                    owner
                });
                self.next_ticks.insert(owner, component.next_tick);
                owner
            })
            .collect();
        for owner in owners.values_mut() {
            *owner = component_owners[*owner];
        }
        self.owners = owners;
        let regions = self.next_ticks.len();
        if previous != regions {
            tracing::trace!(
                previous,
                regions,
                active_chunks = active.len(),
                next_ticks = ?self.next_ticks,
                "Scheduled chunk regions changed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_larger_side_keeps_the_owner_after_a_split() {
        let east = Vector2::new(1, 0);
        let bridge = Vector2::new(0, 0);
        let west = Vector2::new(-1, 0);
        let west_edge = Vector2::new(-2, 0);
        let mut active = FxHashSet::from_iter([east, bridge, west, west_edge]);
        let mut regions = TickRegions::default();
        regions.active_chunks_changed();
        regions.step_scheduled(&active, Vec::new());
        let original_owner = regions.owners[&east];

        active.remove(&bridge);
        regions.active_chunks_changed();
        regions.step_scheduled(&active, Vec::new());
        assert_eq!(regions.owners[&west], original_owner);
        assert_eq!(regions.owners[&west_edge], original_owner);
        assert_ne!(regions.owners[&east], original_owner);

        active.insert(bridge);
        regions.active_chunks_changed();
        regions.step_scheduled(&active, Vec::new());
        assert!(
            regions
                .owners
                .values()
                .all(|&owner| owner == original_owner)
        );
    }
}
