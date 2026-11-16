//! Bounded plugin IPC wall-time measurements. These are not CPU samples.
use sculith_plugin_api::Hook;
use serde::Serialize;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const RECENT: usize = 128;
const DETAILS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Off,
    Monitor,
    Profile,
}

#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub request_id: Option<u64>,
    pub hook: Option<Hook>,
    pub start_ns: u64,
    pub encode_ns: u64,
    pub wall_ns: u64,
    pub sent_bytes: u64,
    pub received_bytes: u64,
    pub failed: bool,
    pub timed_out: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Measurements {
    pub mode: Mode,
    pub collecting: bool,
    pub calls: u64,
    pub failures: u64,
    pub timeouts: u64,
    pub sent_bytes: u64,
    pub received_bytes: u64,
    pub timed_calls: u64,
    pub encode_ns: u64,
    pub wall_ns: u64,
    pub maximum_ns: u64,
    /// Exact nearest-rank quantiles of the most recent 128 timed calls.
    pub recent_p50_ns: Option<u64>,
    pub recent_p95_ns: Option<u64>,
    pub recent_p99_ns: Option<u64>,
    pub recent_count: usize,
    pub overwritten_samples: u64,
    pub samples: Vec<Sample>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginProfile {
    pub name: String,
    pub api_version: u32,
    pub worker_pid: Option<u32>,
    pub disabled: bool,
    pub ipc: Measurements,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub scope: &'static str,
    pub dispatcher: Measurements,
    pub plugins: Vec<PluginProfile>,
}

pub(super) struct Token {
    start: Instant,
    relative_ns: u64,
    detailed: bool,
}
pub(super) struct Collector {
    mode: Mode,
    origin: Option<Instant>,
    deadline: Option<Instant>,
    calls: u64,
    failures: u64,
    timeouts: u64,
    sent: u64,
    received: u64,
    timed: u64,
    encode: u64,
    wall: u64,
    maximum: u64,
    overwritten: u64,
    recent: VecDeque<u64>,
    samples: VecDeque<Sample>,
}

fn ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}
impl Default for Collector {
    fn default() -> Self {
        Self {
            mode: Mode::Off,
            origin: None,
            deadline: None,
            calls: 0,
            failures: 0,
            timeouts: 0,
            sent: 0,
            received: 0,
            timed: 0,
            encode: 0,
            wall: 0,
            maximum: 0,
            overwritten: 0,
            recent: VecDeque::new(),
            samples: VecDeque::new(),
        }
    }
}
impl Collector {
    pub fn configure(&mut self, mode: Mode, origin: Instant, deadline: Option<Instant>) {
        self.mode = mode;
        self.origin = Some(origin);
        self.deadline = deadline;
        self.timed = 0;
        self.encode = 0;
        self.wall = 0;
        self.maximum = 0;
        self.overwritten = 0;
        self.recent = VecDeque::new();
        self.samples = VecDeque::new();
    }
    fn collecting(&self, now: Instant) -> bool {
        self.mode != Mode::Off && self.deadline.is_none_or(|end| now < end)
    }
    pub fn begin(&self) -> Option<Token> {
        if self.mode == Mode::Off {
            return None;
        }
        let now = Instant::now();
        self.collecting(now).then(|| Token {
            start: now,
            relative_ns: ns(now.saturating_duration_since(self.origin.expect("configured"))),
            detailed: self.mode == Mode::Profile,
        })
    }
    pub fn elapsed(token: &Option<Token>) -> u64 {
        token.as_ref().map_or(0, |token| ns(token.start.elapsed()))
    }
    pub fn finish(&mut self, token: Option<Token>, mut sample: Sample) {
        self.calls = self.calls.saturating_add(1);
        self.sent = self.sent.saturating_add(sample.sent_bytes);
        self.received = self.received.saturating_add(sample.received_bytes);
        self.failures = self.failures.saturating_add(u64::from(sample.failed));
        self.timeouts = self.timeouts.saturating_add(u64::from(sample.timed_out));
        let Some(token) = token else { return };
        sample.start_ns = token.relative_ns;
        sample.wall_ns = ns(token.start.elapsed());
        self.record(sample, token.detailed);
    }
    fn record(&mut self, sample: Sample, detailed: bool) {
        self.timed = self.timed.saturating_add(1);
        self.encode = self.encode.saturating_add(sample.encode_ns);
        self.wall = self.wall.saturating_add(sample.wall_ns);
        self.maximum = self.maximum.max(sample.wall_ns);
        if self.recent.len() == RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back(sample.wall_ns);
        if detailed {
            if self.samples.len() == DETAILS {
                self.samples.pop_front();
                self.overwritten = self.overwritten.saturating_add(1);
            }
            self.samples.push_back(sample);
        }
    }
    pub fn snapshot(&self, now: Instant) -> Measurements {
        let mut sorted: Vec<_> = self.recent.iter().copied().collect();
        sorted.sort_unstable();
        let quantile = |percent: usize| {
            (!sorted.is_empty()).then(|| sorted[(sorted.len() * percent).div_ceil(100) - 1])
        };
        Measurements {
            mode: self.mode,
            collecting: self.collecting(now),
            calls: self.calls,
            failures: self.failures,
            timeouts: self.timeouts,
            sent_bytes: self.sent,
            received_bytes: self.received,
            timed_calls: self.timed,
            encode_ns: self.encode,
            wall_ns: self.wall,
            maximum_ns: self.maximum,
            recent_p50_ns: quantile(50),
            recent_p95_ns: quantile(95),
            recent_p99_ns: quantile(99),
            recent_count: sorted.len(),
            overwritten_samples: self.overwritten,
            samples: self.samples.iter().cloned().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(wall_ns: u64) -> Sample {
        Sample {
            request_id: Some(1),
            hook: Some(Hook::Tick),
            start_ns: 0,
            encode_ns: 2,
            wall_ns,
            sent_bytes: 10,
            received_bytes: 20,
            failed: false,
            timed_out: false,
        }
    }
    #[test]
    fn off_has_no_clock_token_or_history_allocation() {
        let mut stats = Collector::default();
        for _ in 0..1000 {
            let token = stats.begin();
            assert!(token.is_none());
            stats.finish(token, sample(0));
        }
        assert_eq!(
            (stats.calls, stats.sent, stats.received, stats.timed),
            (1000, 10000, 20000, 0)
        );
        assert_eq!((stats.recent.capacity(), stats.samples.capacity()), (0, 0));
        assert_eq!(stats.snapshot(Instant::now()).recent_p99_ns, None);
    }
    #[test]
    fn monitor_quantiles_are_exact_for_the_recent_bounded_window() {
        let mut stats = Collector::default();
        stats.configure(Mode::Monitor, Instant::now(), None);
        for time in 1..=200 {
            stats.record(sample(time), false);
        }
        let result = stats.snapshot(Instant::now());
        assert_eq!(result.recent_count, 128);
        assert_eq!(
            (
                result.recent_p50_ns,
                result.recent_p95_ns,
                result.recent_p99_ns
            ),
            (Some(136), Some(194), Some(199))
        );
        assert_eq!(result.maximum_ns, 200);
        assert!(result.samples.is_empty());
    }
    #[test]
    fn detailed_capture_expires_and_counts_overwritten_samples() {
        let mut stats = Collector::default();
        let start = Instant::now() - Duration::from_secs(2);
        stats.configure(Mode::Profile, start, Some(start + Duration::from_secs(1)));
        assert!(stats.begin().is_none());
        for time in 1..=600 {
            stats.record(sample(time), true);
        }
        let result = stats.snapshot(Instant::now());
        assert!(!result.collecting);
        assert_eq!(result.samples.len(), 512);
        assert_eq!(result.overwritten_samples, 88);
        assert_eq!(result.samples.first().unwrap().wall_ns, 89);
        assert_eq!(result.samples.last().unwrap().wall_ns, 600);
        stats.configure(Mode::Off, Instant::now(), None);
        assert_eq!((stats.recent.capacity(), stats.samples.capacity()), (0, 0));
    }
}
