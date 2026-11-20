//! Runs each ready world once per deadline on the existing simulation pool.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    },
    thread::{self, Thread},
    time::{Duration, Instant},
};

use rustc_hash::FxHashMap;
use uuid::Uuid;

use super::Server;

struct WorldClock {
    next_tick: Instant,
    ticks_until_sync: u8,
    last_seen: u64,
}

impl WorldClock {
    const fn new(now: Instant) -> Self {
        Self {
            next_tick: now,
            ticks_until_sync: 20,
            last_seen: 0,
        }
    }

    fn advance(&mut self, now: Instant, period: Duration) -> bool {
        // Catch up one late tick without queueing every missed deadline.
        self.next_tick = if now.duration_since(self.next_tick) >= period {
            now + period
        } else {
            self.next_tick + period
        };
        self.ticks_until_sync -= 1;
        if self.ticks_until_sync == 0 {
            self.ticks_until_sync = 20;
            true
        } else {
            false
        }
    }
}

struct RunningJobs {
    ticker: Thread,
    pending: AtomicUsize,
    longest_nanos: AtomicI64,
}

struct Job(Arc<RunningJobs>);

impl Drop for Job {
    fn drop(&mut self) {
        self.0.pending.fetch_sub(1, Ordering::Release);
        self.0.ticker.unpark();
    }
}

pub(super) struct WorldScheduler {
    clocks: FxHashMap<Uuid, WorldClock>,
    poll_number: u64,
    jobs: Arc<RunningJobs>,
}

impl WorldScheduler {
    pub(super) fn new() -> Self {
        Self {
            clocks: FxHashMap::default(),
            poll_number: 0,
            jobs: Arc::new(RunningJobs {
                ticker: thread::current(),
                pending: AtomicUsize::new(0),
                longest_nanos: AtomicI64::new(0),
            }),
        }
    }

    fn is_idle(&self) -> bool {
        self.jobs.pending.load(Ordering::Acquire) == 0
    }

    /// Used for an explicitly coordinated operation and while shutting down.
    pub(super) fn wait_idle(&self) {
        while !self.is_idle() {
            thread::park();
        }
    }

    pub(super) fn reset_clocks(&mut self) {
        self.clocks.clear();
    }

    pub(super) fn take_longest_tick(&self) -> i64 {
        self.jobs.longest_nanos.swap(0, Ordering::Relaxed)
    }

    pub(super) fn maintain(&self, server: &Arc<Server>) {
        let manual_save = server.storage_checkpoint.manual_capture_requested();
        if manual_save {
            self.wait_idle();
        }
        server
            .storage_checkpoint
            .capture_at_tick_boundary(server, manual_save);
        if !server.tick_rate_manager.is_frozen() {
            server.plugin_manager.tick_loaders(server);
            server
                .player_data_storage
                .tick_at_boundary(server, || self.wait_idle());
        }
    }

    pub(super) fn dispatch_transfers(&self, server: &Arc<Server>) {
        server.for_each_ready_transfer(|transfer, reservation| {
            self.spawn(server, move |server| transfer.apply(server, reservation));
        });
    }

    /// Returns the earliest future deadline. A busy overdue world relies on its completion
    /// waking this ticker. It never prevents another ready world from being dispatched.
    pub(super) fn poll(&mut self, server: &Arc<Server>, period: Duration) -> Option<Duration> {
        let now = Instant::now();
        let worlds = server.worlds.load();
        self.poll_number = self.poll_number.wrapping_add(1);
        let simulate = !server.tick_rate_manager.is_frozen();
        let mut next = None;
        for world in worlds.iter() {
            let clock = self
                .clocks
                .entry(world.uuid)
                .or_insert_with(|| WorldClock::new(now));
            clock.last_seen = self.poll_number;
            let deadline = if clock.next_tick <= now {
                let Some(ownership) = world.try_simulation() else {
                    continue;
                };
                let sync_time = clock.advance(now, period);
                let deadline = clock.next_tick;
                let world = world.clone();
                self.spawn(server, move |server| {
                    server.tick_owned_world(&world, ownership, simulate, sync_time);
                });
                deadline
            } else {
                clock.next_tick
            };
            next = Some(next.map_or(deadline, |earliest: Instant| earliest.min(deadline)));
        }
        self.clocks
            .retain(|_, clock| clock.last_seen == self.poll_number);
        next.map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    fn spawn(&self, server: &Arc<Server>, run: impl FnOnce(&Arc<Server>) + Send + 'static) {
        self.jobs.pending.fetch_add(1, Ordering::Relaxed);
        let job = Job(self.jobs.clone());
        let owner = server.clone();
        server.simulation_pool.spawn_fifo(move || {
            let started = Instant::now();
            if catch_unwind(AssertUnwindSafe(|| run(&owner))).is_err() {
                owner.stop();
            }
            job.0
                .longest_nanos
                .fetch_max(started.elapsed().as_nanos() as i64, Ordering::Relaxed);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::WorldClock;
    use std::time::{Duration, Instant};

    #[test]
    fn a_late_world_drops_missed_deadlines_without_advancing_a_peer() {
        let start = Instant::now();
        let period = Duration::from_millis(50);
        let mut slow = WorldClock::new(start);
        let mut healthy = WorldClock::new(start);

        assert!(!slow.advance(start, period));
        assert!(!healthy.advance(start, period));
        let late = start + period * 24;
        assert!(!slow.advance(late, period));
        assert_eq!(slow.next_tick, late + period);
        assert_eq!(healthy.next_tick, start + period);
        assert_eq!(slow.ticks_until_sync, 18);

        for tick in 1..20 {
            assert_eq!(healthy.advance(start + period * tick, period), tick == 19);
        }
        assert_eq!(healthy.next_tick, start + period * 20);
    }
}
