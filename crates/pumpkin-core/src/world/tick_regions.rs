use std::{cmp::Reverse, collections::hash_map::Entry, sync::Arc};

use pumpkin_world::level::{ScheduledTickGroup, ScheduledTickStep};
use rustc_hash::{FxHashMap, FxHashSet};
use sculith_util::math::vector2::Vector2;

use super::{entity_chunks::RegionalEntityChunks, regional_input::RegionalInput};

pub type RegionId = usize;
pub type RegionChunks = Arc<[Vector2<i32>]>;

/// Published after membership changes. A dispatched job retains its original extent.
#[derive(Default)]
pub struct RegionLayout {
    active: Arc<ActiveRegions>,
    inactive: RegionChunks,
}

#[derive(Default)]
struct ActiveRegions {
    owners: FxHashMap<Vector2<i32>, RegionId>,
    regions: FxHashMap<RegionId, Arc<ActiveRegion>>,
}

struct ActiveRegion {
    chunks: RegionChunks,
    input: RegionalInput,
    entity_chunks: RegionalEntityChunks,
}

impl ActiveRegion {
    fn new(chunks: RegionChunks) -> Self {
        Self {
            chunks,
            input: RegionalInput::default(),
            entity_chunks: RegionalEntityChunks::default(),
        }
    }
}

impl RegionLayout {
    pub(crate) const INACTIVE: RegionId = RegionId::MAX;

    #[cfg(test)]
    pub(crate) fn from_active(
        owners: FxHashMap<Vector2<i32>, RegionId>,
        regions: FxHashMap<RegionId, RegionChunks>,
    ) -> Self {
        let regions = regions
            .into_iter()
            .map(|(id, chunks)| (id, Arc::new(ActiveRegion::new(chunks))))
            .collect();
        Self {
            active: Arc::new(ActiveRegions { owners, regions }),
            inactive: RegionChunks::default(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.active.regions.is_empty() && self.inactive.is_empty()
    }

    pub(super) fn shares_active_regions(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.active, &other.active)
    }

    pub(super) fn activates_chunks_from(&self, previous: &Self) -> bool {
        self.active
            .owners
            .keys()
            .any(|pos| !previous.active.owners.contains_key(pos))
    }

    pub(super) fn active_owner(&self, pos: Vector2<i32>) -> Option<RegionId> {
        self.active.owners.get(&pos).copied()
    }

    pub(crate) fn owner(&self, pos: Vector2<i32>) -> Option<RegionId> {
        self.active_owner(pos).or_else(|| {
            self.inactive
                .binary_search_by_key(&(pos.x, pos.y), |pos| (pos.x, pos.y))
                .ok()
                .map(|_| Self::INACTIVE)
        })
    }

    pub(crate) fn chunks(&self, owner: RegionId) -> Option<&RegionChunks> {
        if owner == Self::INACTIVE {
            return (!self.inactive.is_empty()).then_some(&self.inactive);
        }
        self.active.regions.get(&owner).map(|region| &region.chunks)
    }

    pub(super) fn input(&self, owner: RegionId) -> Option<&RegionalInput> {
        self.active.regions.get(&owner).map(|region| &region.input)
    }

    pub(super) fn entity_chunk_work(&self, owner: RegionId) -> Option<&RegionalEntityChunks> {
        self.active
            .regions
            .get(&owner)
            .map(|region| &region.entity_chunks)
    }

    pub(super) fn entity_chunk_work_at(&self, pos: Vector2<i32>) -> Option<&RegionalEntityChunks> {
        self.entity_chunk_work(*self.active.owners.get(&pos)?)
    }

    pub(super) fn clear_entity_chunk_work(&self) {
        for region in self.active.regions.values() {
            region.entity_chunks.clear();
        }
    }

    pub(super) fn replaced_inputs<'a>(
        &'a self,
        next: &'a Self,
    ) -> impl Iterator<Item = &'a RegionalInput> {
        self.active
            .regions
            .iter()
            .filter_map(move |(owner, region)| {
                (!next
                    .active
                    .regions
                    .get(owner)
                    .is_some_and(|current| Arc::ptr_eq(current, region)))
                .then_some(&region.input)
            })
    }

    pub(crate) fn regions(&self) -> impl Iterator<Item = (RegionId, &RegionChunks)> {
        self.active
            .regions
            .iter()
            .map(|(&id, region)| (id, &region.chunks))
            .chain(
                self.chunks(Self::INACTIVE)
                    .map(|chunks| (Self::INACTIVE, chunks)),
            )
    }
}

#[derive(Default)]
pub(super) struct TickRegions {
    layout: Arc<RegionLayout>,
    next_ticks: FxHashMap<RegionId, u64>,
    next_owner: RegionId,
    needs_rebuild: bool,
}

#[derive(Default)]
struct Component {
    anchor: (i32, i32),
    overlaps: FxHashMap<RegionId, usize>,
    next_tick: u64,
    owner: Option<RegionId>,
    chunks: Vec<Vector2<i32>>,
}

impl TickRegions {
    pub(super) const fn active_chunks_changed(&mut self) {
        self.needs_rebuild = true;
    }

    pub(super) const fn layout(&self) -> &Arc<RegionLayout> {
        &self.layout
    }

    pub(super) fn schedule_ticks(
        &mut self,
        active: &FxHashSet<Vector2<i32>>,
        scheduled: Vec<Vector2<i32>>,
        coordinated: bool,
    ) -> Vec<ScheduledTickGroup> {
        if std::mem::take(&mut self.needs_rebuild) {
            self.rebuild(active);
        }

        let mut inactive = Vec::new();
        let mut groups: FxHashMap<Option<RegionId>, Vec<_>> = FxHashMap::default();
        for chunk in scheduled {
            // Loaded chunks without an active owner still have advancing timers.
            let owner = self.layout.active.owners.get(&chunk).copied();
            if owner.is_none() {
                inactive.push(chunk);
            }
            groups.entry(owner).or_default().push(chunk);
        }
        inactive.sort_unstable_by_key(|pos| (pos.x, pos.y));
        if self.layout.inactive.as_ref() != inactive.as_slice() {
            self.layout = Arc::new(RegionLayout {
                active: self.layout.active.clone(),
                inactive: inactive.into(),
            });
        }

        let plan = groups
            .into_iter()
            .map(|(owner, chunks)| {
                let step = if coordinated {
                    ScheduledTickStep::Advance(owner.map(|owner| self.next_ticks[&owner]))
                } else {
                    ScheduledTickStep::Ready
                };
                ScheduledTickGroup { chunks, step }
            })
            .collect();
        if coordinated {
            for tick in self.next_ticks.values_mut() {
                *tick += 1;
            }
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
                component.chunks.push(chunk);
                if let Some(&owner) = self.layout.active.owners.get(&chunk) {
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
        let mut regions = FxHashMap::default();
        let component_owners: Vec<_> = components
            .into_iter()
            .map(|component| {
                let owner = component.owner.unwrap_or_else(|| {
                    let owner = self.next_owner;
                    self.next_owner += 1;
                    owner
                });
                self.next_ticks.insert(owner, component.next_tick);
                let count = component.chunks.len();
                // In-flight jobs use extent identity to retain their write authority.
                let region = self
                    .layout
                    .active
                    .regions
                    .get(&owner)
                    .filter(|region| {
                        region.chunks.len() == count
                            && component.overlaps.get(&owner) == Some(&count)
                            && !region.input.is_retired()
                    })
                    .cloned()
                    .unwrap_or_else(|| Arc::new(ActiveRegion::new(component.chunks.into())));
                regions.insert(owner, region);
                owner
            })
            .collect();
        for owner in owners.values_mut() {
            *owner = component_owners[*owner];
        }
        self.layout = Arc::new(RegionLayout {
            active: Arc::new(ActiveRegions { owners, regions }),
            inactive: self.layout.inactive.clone(),
        });
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
        regions.schedule_ticks(&active, Vec::new(), true);
        let original_owner = regions.layout.owner(east).unwrap();

        active.remove(&bridge);
        regions.active_chunks_changed();
        regions.schedule_ticks(&active, Vec::new(), true);
        assert_eq!(regions.layout.owner(west), Some(original_owner));
        assert_eq!(regions.layout.owner(west_edge), Some(original_owner));
        assert_ne!(regions.layout.owner(east), Some(original_owner));

        active.insert(bridge);
        regions.active_chunks_changed();
        regions.schedule_ticks(&active, Vec::new(), true);
        assert!(
            regions
                .layout
                .active
                .owners
                .values()
                .all(|&owner| owner == original_owner)
        );
    }

    #[test]
    fn unrelated_extents_survive_a_merge_and_split() {
        let west = Vector2::new(-1, 0);
        let west_edge = Vector2::new(-2, 0);
        let bridge = Vector2::new(0, 0);
        let east = Vector2::new(1, 0);
        let distant = Vector2::new(20, 0);
        let inactive = Vector2::new(40, 0);
        let mut active = FxHashSet::from_iter([west, west_edge, east, distant]);
        let mut regions = TickRegions::default();
        regions.active_chunks_changed();
        regions.schedule_ticks(&active, vec![inactive], true);
        let separate = regions.layout().clone();
        let west_owner = separate.owner(west).unwrap();
        let distant_owner = separate.owner(distant).unwrap();

        active.insert(bridge);
        regions.active_chunks_changed();
        regions.schedule_ticks(&active, vec![inactive], true);
        let merged = regions.layout().clone();
        assert_eq!(merged.owner(east), Some(west_owner));

        active.remove(&bridge);
        regions.active_chunks_changed();
        regions.schedule_ticks(&active, vec![inactive], true);
        let split = regions.layout();
        assert_eq!(split.owner(west), Some(west_owner));
        assert_ne!(split.owner(east), Some(west_owner));

        for owner in [distant_owner, RegionLayout::INACTIVE] {
            let extent = separate.chunks(owner).unwrap();
            assert!(Arc::ptr_eq(extent, merged.chunks(owner).unwrap()));
            assert!(Arc::ptr_eq(extent, split.chunks(owner).unwrap()));
        }
        assert!(std::ptr::eq(
            separate.input(distant_owner).unwrap(),
            split.input(distant_owner).unwrap(),
        ));
        let original = separate.chunks(west_owner).unwrap();
        let combined = merged.chunks(west_owner).unwrap();
        let restored = split.chunks(west_owner).unwrap();
        assert!(!Arc::ptr_eq(original, combined));
        assert!(!Arc::ptr_eq(combined, restored));
        assert!(!Arc::ptr_eq(original, restored));
    }

    #[test]
    fn scheduled_chunks_handoff_between_active_and_inactive_owners() {
        let west = Vector2::new(0, 0);
        let east = Vector2::new(12, 0);
        let mut active = FxHashSet::from_iter([west]);
        let mut regions = TickRegions::default();
        regions.active_chunks_changed();
        let work = regions.schedule_ticks(&active, vec![west, east], false);
        assert!(
            work.iter()
                .all(|work| matches!(work.step, ScheduledTickStep::Ready))
        );
        let before = regions.layout().clone();
        assert_ne!(before.owner(west), before.owner(east));
        assert_eq!(
            before.chunks(RegionLayout::INACTIVE).unwrap().as_ref(),
            [east]
        );

        active.remove(&west);
        active.insert(east);
        regions.active_chunks_changed();
        regions.schedule_ticks(&active, vec![west, east], true);
        let after = regions.layout();
        assert_eq!(after.owner(west), Some(RegionLayout::INACTIVE));
        assert_ne!(after.owner(east), before.owner(east));
        assert_eq!(
            after.chunks(RegionLayout::INACTIVE).unwrap().as_ref(),
            [west]
        );
        assert_eq!(before.owner(east), Some(RegionLayout::INACTIVE));
    }
}
