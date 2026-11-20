use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

use pumpkin_util::math::position::BlockPos;
use rustc_hash::FxHashSet;

use crate::tick::{MAX_TICK_DELAY, OrderedTick, ScheduledTick};

pub struct ChunkTickScheduler<T> {
    inner: Mutex<Option<Box<ChunkTickSchedulerInner<T>>>>,
    offset: AtomicUsize,
}

struct ChunkTickSchedulerInner<T> {
    tick_queue: [Vec<OrderedTick<T>>; MAX_TICK_DELAY],
    ready: Vec<OrderedTick<T>>,
    queued_ticks: FxHashSet<(BlockPos, T)>,
}

impl<T> Default for ChunkTickSchedulerInner<T> {
    fn default() -> Self {
        Self {
            tick_queue: std::array::from_fn(|_| Vec::new()),
            ready: Vec::new(),
            queued_ticks: FxHashSet::default(),
        }
    }
}

impl<'a, T: std::hash::Hash + Eq> ChunkTickScheduler<&'a T> {
    pub fn step_tick(&self) -> Vec<OrderedTick<&'a T>> {
        self.step_tick_at(None)
    }

    /// Advances one simulation tick at the owning region's local time.
    pub fn step_tick_at(&self, region_tick: Option<u64>) -> Vec<OrderedTick<&'a T>> {
        let mut inner_guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.advance_locked(&mut inner_guard, region_tick);
        Self::take_ready_locked(&mut inner_guard)
    }

    /// Advances the timer ring and returns the first ready value, if any.
    pub fn advance(&self) -> Option<&'a T> {
        let mut inner_guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.advance_locked(&mut inner_guard, None);
        inner_guard.as_ref()?.ready.first().map(|tick| tick.value)
    }

    pub(crate) fn apply_ready_batch<R>(
        &self,
        limit: usize,
        apply: impl FnOnce(&[OrderedTick<&'a T>]) -> Option<R>,
    ) -> Option<R> {
        let mut inner_guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ready = &mut inner_guard.as_mut()?.ready;
        if ready.is_empty() || ready.len() > limit {
            return None;
        }
        ready.sort();
        let result = apply(ready)?;
        Self::take_ready_locked(&mut inner_guard);
        Some(result)
    }

    /// Takes ready work at the simulation boundary that will execute it.
    pub fn take_ready(&self) -> Vec<OrderedTick<&'a T>> {
        let mut inner_guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::take_ready_locked(&mut inner_guard)
    }

    fn advance_locked(
        &self,
        inner: &mut Option<Box<ChunkTickSchedulerInner<&'a T>>>,
        region_tick: Option<u64>,
    ) {
        let previous_offset = self.offset.load(Ordering::Relaxed);
        let current_offset = region_tick.map_or(previous_offset, |tick| {
            (tick % MAX_TICK_DELAY as u64) as usize
        });
        let next_offset = (current_offset + 1) % MAX_TICK_DELAY;
        self.offset.store(next_offset, Ordering::Relaxed);
        let Some(inner) = inner else {
            return;
        };

        // Rebase queued delays when a chunk inherits a different region clock.
        let shift = (current_offset + MAX_TICK_DELAY - previous_offset) % MAX_TICK_DELAY;
        if shift != 0 {
            inner.tick_queue.rotate_right(shift);
        }
        let due = &mut inner.tick_queue[current_offset];
        if inner.ready.is_empty() {
            std::mem::swap(&mut inner.ready, due);
        } else {
            inner.ready.append(due);
        }
    }

    fn take_ready_locked(
        scheduler: &mut Option<Box<ChunkTickSchedulerInner<&'a T>>>,
    ) -> Vec<OrderedTick<&'a T>> {
        let Some(inner) = scheduler else {
            return Vec::new();
        };
        let due = std::mem::take(&mut inner.ready);
        for tick in &due {
            inner.queued_ticks.remove(&(tick.position, tick.value));
        }
        if inner.queued_ticks.is_empty() {
            *scheduler = None;
        }
        due
    }

    pub fn schedule_tick(&self, tick: &ScheduledTick<&'a T>, sub_tick_order: u64) {
        let mut inner_guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let offset = self.offset.load(Ordering::Relaxed);
        let inner = inner_guard.get_or_insert_with(Box::default);

        if inner.queued_ticks.insert((tick.position, tick.value)) {
            let index = (offset + tick.delay as usize) % MAX_TICK_DELAY;

            inner.tick_queue[index].push(OrderedTick {
                priority: tick.priority,
                sub_tick_order,
                position: tick.position,
                value: tick.value,
            });
        }
    }

    pub fn is_scheduled(&self, pos: BlockPos, value: &T) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|inner| inner.queued_ticks.contains(&(pos, value)))
    }

    pub fn clear_area(&self, min: &BlockPos, max: &BlockPos) {
        let mut inner_guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(inner) = inner_guard.as_mut() else {
            return;
        };

        let contains = |position: &BlockPos| {
            position.0.x >= min.0.x
                && position.0.x < max.0.x
                && position.0.y >= min.0.y
                && position.0.y < max.0.y
                && position.0.z >= min.0.z
                && position.0.z < max.0.z
        };

        for queue in &mut inner.tick_queue {
            queue.retain(|tick| !contains(&tick.position));
        }
        inner.ready.retain(|tick| !contains(&tick.position));
        inner
            .queued_ticks
            .retain(|(position, _)| !contains(position));
        let became_empty = inner.queued_ticks.is_empty();

        if became_empty {
            *inner_guard = None;
        }
    }

    pub fn has_ticks(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|inner| !inner.queued_ticks.is_empty())
    }

    #[must_use]
    pub fn to_vec(&self) -> Vec<ScheduledTick<&'a T>> {
        let inner_guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let offset = self.offset.load(Ordering::Relaxed);
        let Some(inner) = inner_guard.as_ref() else {
            return Vec::new();
        };

        let mut res: Vec<_> = inner
            .ready
            .iter()
            .map(|tick| ScheduledTick {
                delay: 0,
                priority: tick.priority,
                position: tick.position,
                value: tick.value,
            })
            .collect();

        for i in 0..MAX_TICK_DELAY {
            let index = (offset + i) % MAX_TICK_DELAY;
            res.extend(inner.tick_queue[index].iter().map(|x| ScheduledTick {
                delay: i as u8,
                priority: x.priority,
                position: x.position,
                value: x.value,
            }));
        }
        res
    }
}

impl<'a, T: std::hash::Hash + Eq + 'static> FromIterator<ScheduledTick<&'a T>>
    for ChunkTickScheduler<&'a T>
{
    fn from_iter<I: IntoIterator<Item = ScheduledTick<&'a T>>>(iter: I) -> Self {
        let scheduler = Self::default();
        let iter = iter.into_iter();

        let (lower, _) = iter.size_hint();
        if lower > 0 {
            let mut inner_guard = scheduler
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let inner = inner_guard.get_or_insert_with(Box::default);
            inner.queued_ticks.reserve(lower);
        }

        for tick in iter {
            scheduler.schedule_tick(&tick, 0);
        }
        scheduler
    }
}

impl<T> Default for ChunkTickScheduler<T> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(None),
            offset: AtomicUsize::new(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick::TickPriority;

    #[test]
    fn region_clock_changes_carry_pending_delays_and_priority() {
        let mut scheduler = ChunkTickScheduler::default();
        let position = BlockPos::new(12, 64, -3);
        for (value, delay, priority, order) in [
            (&1, 2, TickPriority::High, 4),
            (&2, 2, TickPriority::Normal, 5),
            (&3, 4, TickPriority::Low, 6),
        ] {
            scheduler.schedule_tick(
                &ScheduledTick {
                    delay,
                    priority,
                    position,
                    value,
                },
                order,
            );
        }

        assert!(scheduler.step_tick_at(Some(250)).is_empty());
        assert!(scheduler.step_tick_at(Some(11)).is_empty());
        let mut due = scheduler.step_tick_at(Some(12));
        due.sort_unstable();
        let result: Vec<_> = due
            .iter()
            .map(|tick| (*tick.value, tick.sub_tick_order))
            .collect();
        assert_eq!(result, [(1, 4), (2, 5)]);

        let saved = scheduler.to_vec();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].delay, 1);
        scheduler = saved.into_iter().collect();
        assert!(scheduler.step_tick_at(Some(255)).is_empty());
        let due = scheduler.step_tick_at(Some(256));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].value, &3);
        assert!(!scheduler.has_ticks());
    }

    #[test]
    fn overdue_work_stays_due_while_waiting_for_its_callback() {
        let scheduler = ChunkTickScheduler::default();
        let position = BlockPos::new(12, 64, -3);
        let tick = ScheduledTick {
            delay: 1,
            priority: TickPriority::High,
            position,
            value: &7,
        };
        scheduler.schedule_tick(&tick, 3);
        scheduler.advance();
        assert!(scheduler.take_ready().is_empty());
        for _ in 0..=MAX_TICK_DELAY {
            scheduler.advance();
        }
        assert!(scheduler.is_scheduled(position, &7));
        scheduler.schedule_tick(&tick, 20);

        let stored = scheduler.to_vec();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].delay, 0);
        let restarted: ChunkTickScheduler<_> = stored.into_iter().collect();
        let restored = restarted.step_tick();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].priority, TickPriority::High);
        let ready = scheduler.take_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].sub_tick_order, 3);
        assert!(scheduler.take_ready().is_empty());
        assert!(!scheduler.has_ticks());
    }

    #[test]
    fn clearing_an_area_cancels_due_work_without_touching_its_neighbour() {
        let scheduler = ChunkTickScheduler::default();
        for x in [12, 13] {
            scheduler.schedule_tick(
                &ScheduledTick {
                    delay: 0,
                    priority: TickPriority::Normal,
                    position: BlockPos::new(x, 64, -3),
                    value: &7,
                },
                3,
            );
        }
        scheduler.advance();
        scheduler.clear_area(&BlockPos::new(12, 64, -3), &BlockPos::new(13, 65, -2));
        let still_due = scheduler.take_ready();
        assert_eq!(still_due.len(), 1);
        assert_eq!(still_due[0].position, BlockPos::new(13, 64, -3));
    }
}
