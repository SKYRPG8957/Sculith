//! Bounded mailbox wall timings. These are not CPU samples or simulation ticks.
use anyhow::{Result, ensure};
use serde::Serialize;
use std::{
    collections::VecDeque,
    num::NonZeroU64,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const RECENT: usize = 128;
const DETAILS: usize = 512;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum Operation {
    PluginWorld,
    PlayerSnapshot,
    PluginInventory,
    InventoryClick,
    Inventory,
    CreativeSlot,
    RegisterCommands,
    SealCommands,
    DisableCommands,
    ListCommands,
    Execute,
    Chat,
    ObservePacket,
    ObservePlayers,
    Snapshot,
    Capture,
    Reserve,
    Activate,
    Retire,
    Move,
    Break,
    Place,
    Select,
    SendMessage,
    ShowModel,
    HideModel,
    PlaySound,
    ShowCustomForm,
    SubmitCustomForm,
    ShowForm,
    SubmitForm,
    PlayerList,
    RouteConsole,
}
impl Operation {
    const COUNT: usize = 33;
    const ALL: [Self; Self::COUNT] = [
        Self::PluginWorld,
        Self::PlayerSnapshot,
        Self::PluginInventory,
        Self::InventoryClick,
        Self::Inventory,
        Self::CreativeSlot,
        Self::RegisterCommands,
        Self::SealCommands,
        Self::DisableCommands,
        Self::ListCommands,
        Self::Execute,
        Self::Chat,
        Self::ObservePacket,
        Self::ObservePlayers,
        Self::Snapshot,
        Self::Capture,
        Self::Reserve,
        Self::Activate,
        Self::Retire,
        Self::Move,
        Self::Break,
        Self::Place,
        Self::Select,
        Self::SendMessage,
        Self::ShowModel,
        Self::HideModel,
        Self::PlaySound,
        Self::ShowCustomForm,
        Self::SubmitCustomForm,
        Self::ShowForm,
        Self::SubmitForm,
        Self::PlayerList,
        Self::RouteConsole,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Off,
    Monitor,
    Profile,
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct Times {
    pub admission_ns: u64,
    pub queue_ns: u64,
    pub service_ns: u64,
    pub total_ns: u64,
}
impl Times {
    fn between(started: u64, queued: u64, processing: u64, end: u64) -> Self {
        let admission_ns = queued.saturating_sub(started);
        let queue_ns = processing.saturating_sub(queued);
        let service_ns = end.saturating_sub(processing);
        Self {
            admission_ns,
            queue_ns,
            service_ns,
            total_ns: admission_ns
                .saturating_add(queue_ns)
                .saturating_add(service_ns),
        }
    }

    fn add(&mut self, other: Self) {
        self.admission_ns = self.admission_ns.saturating_add(other.admission_ns);
        self.queue_ns = self.queue_ns.saturating_add(other.queue_ns);
        self.service_ns = self.service_ns.saturating_add(other.service_ns);
        self.total_ns = self.total_ns.saturating_add(other.total_ns);
    }
    fn maximum(&mut self, other: Self) {
        self.admission_ns = self.admission_ns.max(other.admission_ns);
        self.queue_ns = self.queue_ns.max(other.queue_ns);
        self.service_ns = self.service_ns.max(other.service_ns);
        self.total_ns = self.total_ns.max(other.total_ns);
    }
}
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct Totals {
    pub calls: u64,
    /// Handler abort/unwind, not a gameplay rejection or abandoned reply.
    pub aborted: u64,
    pub sum: Times,
    pub maximum: Times,
}
impl Totals {
    fn record(&mut self, times: Times, aborted: bool) {
        self.calls = self.calls.saturating_add(1);
        self.aborted = self.aborted.saturating_add(u64::from(aborted));
        self.sum.add(times);
        self.maximum.maximum(times);
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub operation: Operation,
    pub start_ns: u64,
    pub times: Times,
    /// Waiting messages observed immediately after receiving this command.
    pub remaining_queue_depth: usize,
    pub aborted: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct OperationProfile {
    pub operation: Operation,
    pub totals: Totals,
}
#[derive(Debug, Clone, Serialize)]
pub struct Recent {
    pub count: usize,
    /// Component-wise exact nearest-rank quantiles; not a single request's phases.
    pub p50: Option<Times>,
    pub p95: Option<Times>,
    pub p99: Option<Times>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub scope: &'static str,
    pub region_id: u64,
    pub mode: Mode,
    pub collecting: bool,
    pub owner_closed: bool,
    pub queue_capacity: usize,
    /// Occupied channel capacity, including sender reservations, excluding service.
    pub occupied_capacity: usize,
    pub completed_commands: u64,
    pub aborted_commands: u64,
    pub totals: Totals,
    pub maximum_remaining_queue_depth: usize,
    pub recent: Recent,
    pub operations: Vec<OperationProfile>,
    pub overwritten_samples: u64,
    pub samples: Vec<Sample>,
}
#[derive(Clone, Copy)]
pub(super) struct Stamp {
    // An active generation is never zero, leaving a niche for untimed messages.
    generation: NonZeroU64,
    started_ns: u64,
    queued_ns: u64,
}
#[derive(Clone)]
struct Window {
    generation: u64,
    mode: Mode,
    origin_ns: u64,
    deadline: Option<Instant>,
    totals: Totals,
    operations: [Totals; Operation::COUNT],
    maximum_depth: usize,
    recent: VecDeque<Times>,
    samples: VecDeque<Sample>,
    overwritten: u64,
}
impl Window {
    fn new(generation: u64, mode: Mode, origin_ns: u64, deadline: Option<Instant>) -> Self {
        Self {
            generation,
            mode,
            origin_ns,
            deadline,
            totals: Totals::default(),
            operations: [Totals::default(); Operation::COUNT],
            maximum_depth: 0,
            recent: VecDeque::new(),
            samples: VecDeque::new(),
            overwritten: 0,
        }
    }
    fn collecting(&self, now: Instant) -> bool {
        self.mode != Mode::Off && self.deadline.is_none_or(|end| now < end)
    }
    fn record(&mut self, sample: Sample) {
        self.totals.record(sample.times, sample.aborted);
        self.operations[sample.operation as usize].record(sample.times, sample.aborted);
        self.maximum_depth = self.maximum_depth.max(sample.remaining_queue_depth);
        if self.recent.len() == RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back(sample.times);
        if self.mode == Mode::Profile {
            if self.samples.len() == DETAILS {
                self.samples.pop_front();
                self.overwritten = self.overwritten.saturating_add(1);
            }
            self.samples.push_back(sample);
        }
    }
}
// Keep owner writes off the cache line repeatedly read by queue producers.
// This is a layout choice, not a CPU-cache-miss measurement.
#[repr(align(64))]
struct Lifetime {
    completed: AtomicU64,
    aborted: AtomicU64,
}
pub(super) struct Shared {
    region_id: u64,
    generation: AtomicU64,
    deadline_ns: AtomicU64,
    lifetime: Lifetime,
    epoch: Instant,
    window: Mutex<Window>,
}
fn ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}
/// Owned exclusively by the game task; only its published view is atomic.
#[derive(Default)]
pub(super) struct Counts {
    completed: u64,
    aborted: u64,
}
impl Shared {
    pub fn new(region_id: u64) -> Self {
        Self {
            region_id,
            generation: AtomicU64::new(0),
            deadline_ns: AtomicU64::new(u64::MAX),
            lifetime: Lifetime {
                completed: AtomicU64::new(0),
                aborted: AtomicU64::new(0),
            },
            epoch: Instant::now(),
            window: Mutex::new(Window::new(0, Mode::Off, 0, None)),
        }
    }
    pub fn configure(&self, mode: Mode, duration: Option<Duration>) -> Result<()> {
        if mode == Mode::Profile {
            ensure!(
                duration.is_some_and(
                    |d| (Duration::from_secs(1)..=Duration::from_secs(300)).contains(&d)
                ),
                "world profile duration must be 1..300 seconds"
            );
        }
        let mut window = self.window.lock().unwrap_or_else(|e| e.into_inner());
        let generation = window
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("world profile generations exhausted"))?;
        let origin = Instant::now();
        let deadline = duration.map(|d| origin + d);
        // Publication of a new nonzero generation makes its deadline visible.
        // A producer racing reconfiguration may omit a timing stamp; the owner
        // still executes the request and rejects stale-generation measurements.
        self.generation.store(0, Ordering::Release);
        *window = Window::new(
            generation,
            mode,
            ns(origin.saturating_duration_since(self.epoch)),
            deadline,
        );
        self.deadline_ns.store(
            deadline.map_or(
                u64::MAX,
                |end| ns(end.saturating_duration_since(self.epoch)),
            ),
            Ordering::Relaxed,
        );
        self.generation.store(
            if mode == Mode::Off { 0 } else { generation },
            Ordering::Release,
        );
        Ok(())
    }
    fn expire_if(&self, generation: u64) {
        // An old producer must never disable a newer capture.
        let _ =
            self.generation
                .compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Relaxed);
    }
    pub fn begin(&self) -> Option<Stamp> {
        let generation = NonZeroU64::new(self.generation.load(Ordering::Acquire))?;
        let now_ns = self.now_ns();
        if now_ns >= self.deadline_ns.load(Ordering::Relaxed) {
            self.expire_if(generation.get());
            return None;
        }
        if self.generation.load(Ordering::Acquire) != generation.get() {
            return None;
        }
        Some(Stamp {
            generation,
            started_ns: now_ns,
            queued_ns: now_ns,
        })
    }
    #[inline]
    fn now_ns(&self) -> u64 {
        ns(self.epoch.elapsed())
    }
    pub fn queued(&self, stamp: &mut Stamp) {
        stamp.queued_ns = self.now_ns();
    }
    pub fn processing<'a>(
        &'a self,
        counts: &'a mut Counts,
        stamp: Option<Stamp>,
        operation: impl FnOnce() -> Operation,
        depth: usize,
    ) -> Work<'a> {
        Work {
            shared: self,
            counts,
            timed: stamp.map(|stamp| (stamp, operation(), self.now_ns(), depth)),
            complete: false,
        }
    }
    pub fn snapshot(&self, capacity: usize, occupied: usize, closed: bool) -> Snapshot {
        // Copy bounded metadata under the lock; sorting and JSON happen outside it.
        let window = self
            .window
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let quantile = |percent: usize| -> Option<Times> {
            if window.recent.is_empty() {
                return None;
            }
            let rank = (window.recent.len() * percent).div_ceil(100) - 1;
            let component = |get: fn(&Times) -> u64| {
                let mut values = window.recent.iter().map(get).collect::<Vec<_>>();
                values.sort_unstable();
                values[rank]
            };
            Some(Times {
                admission_ns: component(|t| t.admission_ns),
                queue_ns: component(|t| t.queue_ns),
                service_ns: component(|t| t.service_ns),
                total_ns: component(|t| t.total_ns),
            })
        };
        Snapshot {
            schema_version: 1,
            scope: "world_mailbox_wall_time",
            region_id: self.region_id,
            mode: window.mode,
            collecting: window.collecting(Instant::now()),
            owner_closed: closed,
            queue_capacity: capacity,
            occupied_capacity: occupied,
            completed_commands: self.lifetime.completed.load(Ordering::Relaxed),
            aborted_commands: self.lifetime.aborted.load(Ordering::Relaxed),
            totals: window.totals,
            maximum_remaining_queue_depth: window.maximum_depth,
            recent: Recent {
                count: window.recent.len(),
                p50: quantile(50),
                p95: quantile(95),
                p99: quantile(99),
            },
            operations: Operation::ALL
                .into_iter()
                .filter_map(|operation| {
                    let totals = window.operations[operation as usize];
                    (totals.calls != 0).then_some(OperationProfile { operation, totals })
                })
                .collect(),
            overwritten_samples: window.overwritten,
            samples: window.samples.into_iter().collect(),
        }
    }
}
pub(super) struct Work<'a> {
    shared: &'a Shared,
    counts: &'a mut Counts,
    timed: Option<(Stamp, Operation, u64, usize)>,
    complete: bool,
}
impl Work<'_> {
    pub fn complete(&mut self) {
        self.complete = true;
    }
    fn publish_counts(&self) {
        self.shared
            .lifetime
            .completed
            .store(self.counts.completed, Ordering::Relaxed);
        if !self.complete {
            self.shared
                .lifetime
                .aborted
                .store(self.counts.aborted, Ordering::Relaxed);
        }
    }
}
impl Drop for Work<'_> {
    fn drop(&mut self) {
        self.counts.completed = self.counts.completed.saturating_add(1);
        if !self.complete {
            self.counts.aborted = self.counts.aborted.saturating_add(1);
        }
        if let Some((stamp, operation, processing, depth)) = self.timed {
            let end = self.shared.now_ns();
            let mut window = self.shared.window.lock().unwrap_or_else(|e| e.into_inner());
            // Reconfiguration discards measurements, never game work. Publishing
            // completion under this lock makes a subsequent snapshot include the
            // finished timing record before it observes that command as ended.
            if window.generation == stamp.generation.get() && window.mode != Mode::Off {
                let times = Times::between(stamp.started_ns, stamp.queued_ns, processing, end);
                let start_ns = stamp.started_ns.saturating_sub(window.origin_ns);
                window.record(Sample {
                    operation,
                    start_ns,
                    times,
                    remaining_queue_depth: depth,
                    aborted: !self.complete,
                });
            }
            self.publish_counts();
        } else {
            self.publish_counts();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(time: u64) -> Sample {
        Sample {
            operation: Operation::Snapshot,
            start_ns: time,
            times: Times {
                admission_ns: time,
                queue_ns: time * 2,
                service_ns: time * 3,
                total_ns: time * 6,
            },
            remaining_queue_depth: 2,
            aborted: false,
        }
    }
    #[tokio::test]
    async fn retiring_a_lease_never_waits_for_the_profile_lock() {
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let (world, owner) = crate::create(shutdown);
        let owner = tokio::spawn(owner);
        let lease = world
            .reserve(
                sculith_game::Profile {
                    subject: "offline:lease-meter".into(),
                    name: "LeaseMeter".into(),
                    uuid: uuid::Uuid::from_u128(7),
                    edition: sculith_game::Edition::Bedrock,
                },
                Default::default(),
            )
            .await
            .unwrap();
        world.start_profile(Duration::from_secs(30)).unwrap();
        let lock = world.telemetry.window.lock().unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            drop(lease);
            sent.send(()).unwrap();
        });
        let retired = received.recv_timeout(Duration::from_secs(1));
        drop(lock);
        assert!(retired.is_ok(), "lease Drop waited for optional telemetry");
        thread.join().unwrap();
        world.capture().await.unwrap();
        stop.send_replace(true);
        owner.await.unwrap().unwrap();
    }
    #[test]
    fn single_owner_publication_saturates_without_atomic_read_modify_write() {
        let shared = Shared::new(0);
        let mut counts = Counts {
            completed: u64::MAX - 1,
            aborted: u64::MAX - 1,
        };
        for _ in 0..2 {
            let _aborted =
                shared.processing(&mut counts, None, || panic!("Off classified a request"), 0);
        }
        let snapshot = shared.snapshot(32, 0, false);
        assert_eq!(
            (snapshot.completed_commands, snapshot.aborted_commands),
            (u64::MAX, u64::MAX)
        );
    }
    #[test]
    fn admission_never_waits_for_the_history_lock() {
        let shared = std::sync::Arc::new(Shared::new(0));
        shared.configure(Mode::Monitor, None).unwrap();
        let lock = shared.window.lock().unwrap();
        let producer = shared.clone();
        let (sent, received) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || sent.send(producer.begin().is_some()).unwrap());
        let stamped = received.recv_timeout(Duration::from_secs(1));
        drop(lock);
        assert!(stamped.unwrap());
        thread.join().unwrap();
    }
    #[test]
    fn late_expiry_cannot_disable_a_new_capture_or_monitor() {
        let shared = Shared::new(0);
        shared
            .configure(Mode::Profile, Some(Duration::from_secs(1)))
            .unwrap();
        let old = shared.begin().unwrap().generation;
        shared.configure(Mode::Monitor, None).unwrap();
        shared.expire_if(old.get());
        let monitor = shared.begin().unwrap().generation;
        assert!(monitor > old);
        shared
            .configure(Mode::Profile, Some(Duration::from_secs(1)))
            .unwrap();
        shared.expire_if(monitor.get());
        assert!(shared.begin().unwrap().generation > monitor);
    }
    #[test]
    fn compact_timestamps_preserve_phase_units_and_capture_generations() {
        let mut counts = Counts::default();
        let times = Times::between(1_000, 1_025, 1_075, 1_100);
        assert_eq!(
            (
                times.admission_ns,
                times.queue_ns,
                times.service_ns,
                times.total_ns
            ),
            (25, 50, 25, 100)
        );
        let times = Times::between(u64::MAX - 100, u64::MAX - 75, u64::MAX - 25, u64::MAX);
        assert_eq!(
            (
                times.admission_ns,
                times.queue_ns,
                times.service_ns,
                times.total_ns
            ),
            (25, 50, 25, 100)
        );
        let shared = Shared::new(0);
        shared
            .configure(Mode::Profile, Some(Duration::from_secs(30)))
            .unwrap();
        let mut stamp = shared.begin().unwrap();
        shared.queued(&mut stamp);
        assert!(stamp.queued_ns >= stamp.started_ns);
        let old_generation = stamp.generation;
        shared
            .configure(Mode::Profile, Some(Duration::from_secs(30)))
            .unwrap();
        assert!(shared.begin().unwrap().generation > old_generation);
        {
            let mut work = shared.processing(&mut counts, Some(stamp), || Operation::Capture, 0);
            work.complete();
        }
        assert_eq!(shared.snapshot(32, 0, false).totals.calls, 0);
        // A command starting exactly at the current capture origin reports zero,
        // even though this owner's monotonic epoch predates both captures.
        let origin_ns = shared.window.lock().unwrap().origin_ns;
        let sample = Stamp {
            generation: shared.begin().unwrap().generation,
            started_ns: origin_ns,
            queued_ns: origin_ns,
        };
        {
            let mut work = shared.processing(&mut counts, Some(sample), || Operation::Snapshot, 0);
            work.complete();
        }
        let snapshot = shared.snapshot(32, 0, false);
        assert_eq!(snapshot.samples.len(), 1);
        assert_eq!(snapshot.samples[0].start_ns, 0);
        assert_eq!(snapshot.samples[0].times.admission_ns, 0);
        // Exhaustion refuses reconfiguration without wrapping to an untimed niche.
        shared.window.lock().unwrap().generation = u64::MAX;
        shared.generation.store(u64::MAX, Ordering::Release);
        assert!(
            shared
                .configure(Mode::Profile, Some(Duration::from_secs(30)))
                .is_err()
        );
        assert_eq!(shared.begin().unwrap().generation.get(), u64::MAX);
    }
    #[test]
    fn off_has_no_clock_token_or_history_and_does_not_classify_payloads() {
        let mut counts = Counts::default();
        let shared = Shared::new(7);
        for _ in 0..1000 {
            assert!(shared.begin().is_none());
            let mut work =
                shared.processing(&mut counts, None, || panic!("Off classified a request"), 0);
            work.complete();
        }
        let window = shared.window.lock().unwrap();
        assert_eq!(
            (window.recent.capacity(), window.samples.capacity()),
            (0, 0)
        );
        drop(window);
        assert_eq!(shared.snapshot(32, 0, false).completed_commands, 1000);
        assert!(std::mem::size_of::<Option<Stamp>>() <= 24);
        assert_ne!(
            std::mem::offset_of!(Shared, generation) / 64,
            std::mem::offset_of!(Shared, lifetime) / 64
        );

        println!(
            "profiling layout: stamp={} optional_stamp={} envelope={} shared={}",
            std::mem::size_of::<Stamp>(),
            std::mem::size_of::<Option<Stamp>>(),
            std::mem::size_of::<crate::Envelope>(),
            std::mem::size_of::<Shared>()
        );
    }
    #[test]
    fn bounded_history_exact_component_quantiles_and_window_reset() {
        let mut counts = Counts::default();
        let shared = Shared::new(0);
        shared.configure(Mode::Monitor, None).unwrap();
        {
            let mut window = shared.window.lock().unwrap();
            for n in 1..=200 {
                window.record(sample(n));
            }
        }
        let snapshot = shared.snapshot(32, 0, false);
        assert_eq!(snapshot.recent.count, 128);
        assert_eq!(snapshot.recent.p50.unwrap().queue_ns, 272);
        assert_eq!(snapshot.recent.p95.unwrap().service_ns, 582);
        assert_eq!(snapshot.recent.p99.unwrap().total_ns, 1194);
        assert!(snapshot.samples.is_empty());
        shared
            .configure(Mode::Profile, Some(Duration::from_secs(1)))
            .unwrap();
        {
            let mut window = shared.window.lock().unwrap();
            for n in 1..=600 {
                window.record(sample(n));
            }
        }
        let snapshot = shared.snapshot(32, 0, false);
        assert_eq!(
            (snapshot.samples.len(), snapshot.overwritten_samples),
            (512, 88)
        );
        assert_eq!(snapshot.samples[0].start_ns, 89);
        let old = shared.begin().unwrap();
        shared.configure(Mode::Off, None).unwrap();
        {
            let mut work = shared.processing(&mut counts, Some(old), || Operation::Capture, 0);
            work.complete();
        }
        assert_eq!(shared.snapshot(32, 0, false).totals.calls, 0);
        let window = shared.window.lock().unwrap();
        assert_eq!(
            (window.recent.capacity(), window.samples.capacity()),
            (0, 0)
        );
    }
    #[test]
    fn expiry_stops_new_clock_tokens_and_unwind_is_distinct_from_rejection() {
        let mut counts = Counts::default();
        let shared = Shared::new(0);
        for duration in [Duration::ZERO, Duration::from_secs(301)] {
            assert!(shared.configure(Mode::Profile, Some(duration)).is_err());
        }
        shared
            .configure(Mode::Profile, Some(Duration::from_secs(1)))
            .unwrap();
        {
            let _work = shared.processing(&mut counts, shared.begin(), || Operation::Capture, 0);
        }
        assert_eq!(shared.snapshot(32, 0, false).aborted_commands, 1);
        assert!(shared.snapshot(32, 0, false).samples[0].aborted);
        shared.window.lock().unwrap().deadline = Some(Instant::now() - Duration::from_secs(1));
        shared.deadline_ns.store(0, Ordering::Relaxed);
        assert!(shared.begin().is_none());
        assert_eq!(shared.generation.load(Ordering::Acquire), 0);
    }
}
