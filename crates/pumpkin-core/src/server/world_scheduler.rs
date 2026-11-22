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

use super::{
    PlayerCommands, Server, WorldStep,
    region_scheduler::{RegionSchedule, TickDeadline, next_deadline},
};

struct WorldClock {
    next_tick: Instant,
    ticks_until_sync: u8,
    last_seen: u64,
    regions: RegionSchedule,
}

impl WorldClock {
    fn new(now: Instant) -> Self {
        Self {
            next_tick: now,
            ticks_until_sync: 20,
            last_seen: 0,
            regions: RegionSchedule::default(),
        }
    }

    fn advance(&mut self, now: Instant, period: Duration) -> bool {
        self.next_tick = next_deadline(self.next_tick, now, period);
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
    player_commands: PlayerCommands,
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
                player_commands: PlayerCommands::default(),
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

    pub(super) fn run_player_commands(&self, server: &Arc<Server>) {
        if !server.stop_token.is_cancelled() && self.jobs.player_commands.has_pending() {
            self.wait_idle();
            self.jobs.player_commands.run_pending(server);
        }
    }

    pub(super) fn tick_coordinated(&self, server: &Arc<Server>) {
        server.tick_with_player_commands(&self.jobs.player_commands);
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
            self.jobs
                .spawn(server, move |server| transfer.apply(server, reservation));
        });
    }

    /// Returns the earliest future deadline. A busy owner wakes this ticker when its
    /// work finishes. Ready regions and worlds can be dispatched in the meantime.
    pub(super) fn poll(&mut self, server: &Arc<Server>, period: Duration) -> Option<Duration> {
        let now = Instant::now();
        let worlds = server.worlds.load();
        self.poll_number = self.poll_number.wrapping_add(1);
        let frozen = server.tick_rate_manager.is_frozen();
        let mut next = None;
        for world in worlds.iter() {
            let clock = self
                .clocks
                .entry(world.uuid)
                .or_insert_with(|| WorldClock::new(now));
            clock.last_seen = self.poll_number;
            if !frozen {
                let region_deadline = clock.regions.poll(
                    world.region_layout.load_full(),
                    now,
                    period,
                    |owner, chunks, deadline, permit| {
                        let Ok(activity) = world.region_activity.clone().try_read_owned() else {
                            return false;
                        };
                        let world = world.clone();
                        self.jobs.spawn(server, move |server| {
                            let _activity = activity;
                            let _permit = permit;
                            if !server.stop_token.is_cancelled() {
                                world.advance_region_time(owner, &chunks, deadline);
                                tracing::trace!(world = %world.uuid, region = owner, "Regional chunk time step finished");
                            }
                        });
                        true
                    },
                );
                next = next.into_iter().chain(region_deadline).min();
            }
            if clock.next_tick <= now {
                let Some(ownership) = world.try_simulation() else {
                    continue;
                };
                let mode = if frozen {
                    WorldStep::NetworkOnly
                } else {
                    WorldStep::RegionalDeadlines(TickDeadline {
                        at: clock.next_tick,
                        period,
                    })
                };
                let sync_time = clock.advance(now, period);
                let world = world.clone();
                let jobs = self.jobs.clone();
                self.jobs.spawn(server, move |server| {
                    server.tick_owned_world(
                        &world,
                        ownership,
                        mode,
                        sync_time,
                        &jobs.player_commands,
                    );
                });
            }
            next = Some(next.map_or(clock.next_tick, |earliest: Instant| {
                earliest.min(clock.next_tick)
            }));
        }
        self.clocks
            .retain(|_, clock| clock.last_seen == self.poll_number);
        next.map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }
}

impl RunningJobs {
    fn spawn(
        self: &Arc<Self>,
        server: &Arc<Server>,
        run: impl FnOnce(&Arc<Server>) + Send + 'static,
    ) {
        self.pending.fetch_add(1, Ordering::Relaxed);
        let job = Job(self.clone());
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
