//! Deadlines and exclusive chunk extents for independently dispatched region jobs.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::world::{RegionChunks, RegionId, RegionLayout};

pub(super) fn next_deadline(deadline: Instant, now: Instant, period: Duration) -> Instant {
    // Drop missed starts instead of queuing a backlog when an owner is late.
    if now.duration_since(deadline) >= period {
        now + period
    } else {
        deadline + period
    }
}

pub(super) struct RegionPermit(Arc<AtomicBool>);

impl Drop for RegionPermit {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

struct RegionClock {
    deadline: Instant,
    waiting: Vec<Arc<AtomicBool>>,
}

struct Flight {
    chunks: RegionChunks,
    finished: Arc<AtomicBool>,
    deadline: Instant,
}

#[derive(Default)]
pub(super) struct RegionSchedule {
    layout: Arc<RegionLayout>,
    clocks: FxHashMap<RegionId, RegionClock>,
    in_flight: Vec<Flight>,
}

impl RegionSchedule {
    fn reconcile(&mut self, layout: Arc<RegionLayout>, now: Instant) {
        let mut deadlines = FxHashMap::default();
        for (chunk, &owner) in &layout.owners {
            let Some(previous) = self.layout.owners.get(chunk) else {
                continue;
            };
            let previous = self.clocks[previous].deadline;
            deadlines
                .entry(owner)
                .and_modify(|deadline: &mut Instant| *deadline = (*deadline).max(previous))
                .or_insert(previous);
        }

        self.clocks = layout
            .regions
            .keys()
            .map(|&owner| {
                let deadline = deadlines.get(&owner).copied().unwrap_or(now);
                (
                    owner,
                    RegionClock {
                        deadline,
                        waiting: Vec::new(),
                    },
                )
            })
            .collect();

        // A predecessor may still be running through several successive topology
        // changes. Wait only where its original chunk extent intersects the new one.
        for flight in &self.in_flight {
            let successors: FxHashSet<_> = flight
                .chunks
                .iter()
                .filter_map(|chunk| layout.owners.get(chunk).copied())
                .collect();
            for owner in successors {
                if let Some(clock) = self.clocks.get_mut(&owner) {
                    clock.waiting.push(flight.finished.clone());
                    clock.deadline = clock.deadline.max(flight.deadline);
                }
            }
        }
        self.layout = layout;
    }

    pub(super) fn poll(
        &mut self,
        layout: Arc<RegionLayout>,
        now: Instant,
        period: Duration,
        mut dispatch: impl FnMut(RegionId, RegionChunks, RegionPermit) -> bool,
    ) -> Option<Instant> {
        self.in_flight
            .retain(|flight| !flight.finished.load(Ordering::Acquire));
        if !Arc::ptr_eq(&layout, &self.layout) {
            self.reconcile(layout, now);
        }
        let mut next = None;
        for (&owner, clock) in &mut self.clocks {
            if clock.deadline <= now {
                clock
                    .waiting
                    .retain(|finished| !finished.load(Ordering::Acquire));
                if !clock.waiting.is_empty() {
                    continue;
                }
                let chunks = self.layout.regions[&owner].clone();
                let finished = Arc::new(AtomicBool::new(false));
                if !dispatch(owner, chunks.clone(), RegionPermit(finished.clone())) {
                    continue;
                }
                clock.waiting.push(finished.clone());
                clock.deadline = next_deadline(clock.deadline, now, period);
                self.in_flight.push(Flight {
                    chunks,
                    finished,
                    deadline: clock.deadline,
                });
            }
            next = Some(next.map_or(clock.deadline, |earliest: Instant| {
                earliest.min(clock.deadline)
            }));
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use pumpkin_util::math::vector2::Vector2;

    use super::*;

    fn layout(regions: &[(RegionId, &[i32])]) -> Arc<RegionLayout> {
        let mut layout = RegionLayout::default();
        for &(owner, xs) in regions {
            let chunks: RegionChunks = xs.iter().map(|&x| Vector2::new(x, 0)).collect();
            layout.owners.extend(chunks.iter().map(|&pos| (pos, owner)));
            layout.regions.insert(owner, chunks);
        }
        Arc::new(layout)
    }

    #[test]
    fn a_blocked_region_does_not_stop_the_other_regions_step() {
        use std::sync::{atomic::AtomicU64, mpsc};
        let mut schedule = RegionSchedule::default();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        let layout = layout(&[(0, &[0]), (1, &[100])]);
        let slow = Arc::new(AtomicU64::new(0));
        let peer = Arc::new(AtomicU64::new(0));
        let (release, pause) = mpsc::sync_channel::<()>(1);
        let (progress, observed) = mpsc::channel();
        let mut pause = Some(pause);
        let mut dispatch = |owner, _chunks, permit| {
            let progress = progress.clone();
            let pause = if owner == 0 { pause.take() } else { None };
            let count = if owner == 0 {
                slow.clone()
            } else {
                peer.clone()
            };
            pool.spawn_fifo(move || {
                if let Some(pause) = pause {
                    let _ = pause.recv_timeout(Duration::from_secs(5));
                }
                count.fetch_add(1, Ordering::Relaxed);
                drop(permit);
                let _ = progress.send(owner);
            });
            true
        };
        let start = Instant::now();
        let period = Duration::from_millis(50);
        schedule.poll(layout.clone(), start, period, &mut dispatch);
        assert_eq!(observed.recv_timeout(Duration::from_secs(2)).unwrap(), 1);
        schedule.poll(layout, start + period * 2, period, &mut dispatch);
        assert_eq!(observed.recv_timeout(Duration::from_secs(2)).unwrap(), 1);
        assert_eq!(slow.load(Ordering::Relaxed), 0);
        assert_eq!(peer.load(Ordering::Relaxed), 2);
        release.send(()).unwrap();
        assert_eq!(observed.recv_timeout(Duration::from_secs(2)).unwrap(), 0);
    }

    #[test]
    fn an_overdue_job_holds_its_extent_through_a_merge_and_split() {
        let mut schedule = RegionSchedule::default();
        let start = Instant::now();
        let period = Duration::from_millis(50);
        let mut slow = None;
        let mut turns = Vec::new();

        schedule.poll(
            layout(&[(0, &[0, 1]), (1, &[100])]),
            start,
            period,
            |owner, _, permit| {
                if owner == 0 {
                    slow = Some(permit);
                }
                turns.push(owner);
                true
            },
        );
        turns.sort_unstable();
        assert_eq!(turns, [0, 1]);
        turns.clear();

        // Both overlapping successors still wait for the original owner, including
        // the split child whose ID differs. Chunk 100 advances on its own deadline.
        let merged = layout(&[(0, &[0, 1, 2]), (1, &[100])]);
        let split = layout(&[(0, &[0]), (2, &[1, 2]), (1, &[100])]);
        for (periods, layout) in [(3, merged), (4, split.clone())] {
            let now = start + period * periods;
            let next = schedule.poll(layout, now, period, |owner, _, _| {
                turns.push(owner);
                true
            });
            assert_eq!(turns, [1]);
            assert_eq!(next, Some(now + period));
            turns.clear();
        }
        drop(slow.take());
        let late = start + period * 4;
        schedule.poll(split, late, period, |owner, _, _| {
            turns.push(owner);
            true
        });
        turns.sort_unstable();
        assert_eq!(turns, [0, 2]);
        assert_eq!(schedule.clocks[&0].deadline, late + period);
        assert_eq!(schedule.clocks[&2].deadline, late + period);
    }
}
