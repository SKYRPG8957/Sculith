use super::server_test_manager::{drain_game_test_queue, has_pending_game_tests};
use super::world_scheduler::WorldScheduler;

use crate::{
    command::CommandSender,
    plugin::server::{
        server_command::ServerCommandEvent, server_tick_end::ServerTickEndEvent,
        server_tick_start::ServerTickStartEvent,
    },
    server::Server,
};
use pumpkin_gametest::GameTestRunner;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

const NANOSECONDS_PER_MILLISECOND: i64 = 1_000_000;
const NANOSECONDS_PER_SECOND: i64 = 1_000_000_000;
/// Vanilla `OVERLOADED_THRESHOLD_NANOS` (`20 * 1s / 20` = 1s).
const OVERLOADED_THRESHOLD_NANOS: i64 = 20 * NANOSECONDS_PER_SECOND / 20;
/// Vanilla `OVERLOADED_TICKS_THRESHOLD`.
const OVERLOADED_TICKS_THRESHOLD: i64 = 20;
/// Vanilla `OVERLOADED_WARNING_INTERVAL_NANOS`.
const OVERLOADED_WARNING_INTERVAL_NANOS: i64 = 10 * NANOSECONDS_PER_SECOND;
/// Vanilla `OVERLOADED_TICKS_WARNING_INTERVAL`.
const OVERLOADED_TICKS_WARNING_INTERVAL: i64 = 100;
/// Vanilla's absolute clock starts far past `lastOverloadWarningNanos = 0`,
/// so the first overload always passes the warning-interval check.
const NEVER_WARNED: i64 = i64::MIN / 2;

/// Tick deadline and overload-warning clock, in nanoseconds since the loop base.
#[derive(Clone, Copy)]
struct TickSchedule {
    next_tick_nanos: i64,
    last_overload_warning_nanos: i64,
}

impl TickSchedule {
    const fn new() -> Self {
        Self {
            next_tick_nanos: 0,
            last_overload_warning_nanos: NEVER_WARNED,
        }
    }
}

pub struct Ticker;

thread_local! {
    static TICKER_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn is_ticker_thread() -> bool {
    TICKER_THREAD.get()
}

pub(crate) const CONSOLE_COMMAND_CAPACITY: usize = 16;

pub(crate) struct ConsoleCommand {
    pub(crate) text: String,
    pub(crate) done: Option<oneshot::Sender<()>>,
}

impl ConsoleCommand {
    fn execute(self, server: &Arc<Server>) {
        let mut event = ServerCommandEvent::new(self.text.clone());
        server.plugin_manager.fire_blocking(server, &mut event);
        if !event.cancelled {
            let _runtime = server.runtime.enter();
            server
                .command_dispatcher
                .load()
                .handle_command(&CommandSender::Console.into_source(server), &self.text);
        }
        if let Some(done) = self.done {
            let _ = done.send(());
        }
    }
}

impl Ticker {
    /// Runs the main server tick loop on a dedicated thread.
    pub fn run(server: &Arc<Server>) {
        Self::run_with_console(server, None);
    }

    pub(crate) fn run_with_console(
        server: &Arc<Server>,
        mut commands: Option<mpsc::Receiver<ConsoleCommand>>,
    ) {
        let previous_context = TICKER_THREAD.replace(true);
        // Single monotonic base, the schedule is plain nanosecond arithmetic.
        let base = Instant::now();
        let mut schedule = TickSchedule::new();
        let mut game_test_runner = GameTestRunner::new();
        let mut worlds = WorldScheduler::new();

        let park_thread = std::thread::current();
        let stop = server.stop_token.clone();
        server.runtime.spawn(async move {
            stop.cancelled().await;
            park_thread.unpark();
        });

        loop {
            run_console_commands(server, &worlds, commands.as_mut());
            if server.stop_token.is_cancelled() {
                break;
            }

            // Doubles as the tick-duration start, so the loop reads the clock twice.
            let tick_start_nanos = elapsed_nanos(base);
            let manager = &server.tick_rate_manager;
            let sprinting = manager.is_sprinting();
            let this_tick_nanos = if sprinting {
                0
            } else {
                manager.nanoseconds_per_tick()
            };

            // Vanilla `MinecraftServer.runServer`: skip ticks when more than
            // `OVERLOADED_THRESHOLD + 20 * nanosecondsPerTick` behind (~2s at 20 TPS).
            let (updated, skipped) =
                apply_overload_skip(schedule, tick_start_nanos, this_tick_nanos);
            schedule = updated;
            if let Some((behind_ms, ticks)) = skipped {
                warn!(
                    "Can't keep up! Is the server overloaded? Running {behind_ms}ms or {ticks} ticks behind"
                );
            }

            // Deadline for the next wait. Work below may finish late; then the
            // following iteration catch-up-runs with no park.
            schedule.next_tick_nanos += this_tick_nanos;

            let tick_number = server.tick_count.load(Ordering::Relaxed);
            let has_game_tests = has_pending_game_tests() || !game_test_runner.is_empty();
            let coordinated =
                sprinting || needs_coordinated_tick(server, tick_number, has_game_tests);
            if coordinated {
                worlds.wait_idle();
                worlds.reset_clocks();
            }
            manager.tick();

            if coordinated && server.plugin_manager.has_handlers::<ServerTickStartEvent>() {
                server
                    .plugin_manager
                    .fire_blocking(server, &mut ServerTickStartEvent::new(tick_number));
            }

            let should_tick_game_tests =
                coordinated && has_game_tests && (manager.runs_normally() || sprinting);
            let period = Duration::from_nanos(this_tick_nanos as u64);

            if sprinting {
                manager.start_sprint_tick_work();
                server.tick();

                if manager.end_sprint_tick_work() {
                    manager.finish_tick_sprint(server);
                }
            } else if coordinated {
                server.tick();
            } else {
                worlds.maintain(server);
                worlds.dispatch_transfers(server);
                let _ = worlds.poll(server, period);
            }

            if should_tick_game_tests {
                server.runtime.block_on(async {
                    drain_game_test_queue(server, &mut game_test_runner).await;
                    game_test_runner.tick().await;
                });
            }

            let tick_duration_nanos = elapsed_nanos(base) - tick_start_nanos;

            let tick_number = server.tick_count.load(Ordering::Relaxed);
            if coordinated && server.plugin_manager.has_handlers::<ServerTickEndEvent>() {
                server.plugin_manager.fire_blocking(
                    server,
                    &mut ServerTickEndEvent::new(tick_number, tick_duration_nanos),
                );
            }

            server.update_tick_times(tick_duration_nanos.max(worlds.take_longest_tick()));

            if server.stop_token.is_cancelled() {
                break;
            }

            wait_until_next_tick(
                server,
                base,
                schedule.next_tick_nanos,
                commands.as_mut(),
                &mut worlds,
                (!coordinated).then_some(period),
            );

            if server.stop_token.is_cancelled() {
                break;
            }
        }

        server.cancel_pending_simulation_changes();
        worlds.wait_idle();
        TICKER_THREAD.set(previous_context);
        debug!("Ticker stopped");
    }
}

fn needs_coordinated_tick(server: &Server, tick_number: i32, has_game_tests: bool) -> bool {
    let manager = &server.tick_rate_manager;
    manager.is_stepping_forward()
        || has_game_tests
        || server.plugin_manager.has_handlers::<ServerTickStartEvent>()
        || server.plugin_manager.has_handlers::<ServerTickEndEvent>()
        || (!manager.is_frozen()
            && (server.datapack_manager.has_tick_functions()
                || server.scheduled_functions.has_due(tick_number as u64)))
}

fn run_console_commands(
    server: &Arc<Server>,
    worlds: &WorldScheduler,
    commands: Option<&mut mpsc::Receiver<ConsoleCommand>>,
) {
    let Some(commands) = commands else {
        return;
    };
    // Give console input time even while sprinting or catching up.
    for _ in 0..CONSOLE_COMMAND_CAPACITY {
        if server.stop_token.is_cancelled() {
            break;
        }
        let Ok(command) = commands.try_recv() else {
            break;
        };
        worlds.wait_idle();
        command.execute(server);
    }
}

/// Vanilla `MinecraftServer.runServer` overload skip. Returns the updated schedule and,
/// when the deadline jumped, the `(behind_ms, ticks)` log payload.
///
/// Skip and warning share one gate, as in vanilla: at most one skip per
/// 10s + 100 ticks of deadline. In between, deadline stays behind and missed
/// ticks run back to back. Accepted lag for vanilla timing parity.
const fn apply_overload_skip(
    schedule: TickSchedule,
    now_nanos: i64,
    this_tick_nanos: i64,
) -> (TickSchedule, Option<(i64, i64)>) {
    let TickSchedule {
        next_tick_nanos,
        last_overload_warning_nanos,
    } = schedule;

    // Sprint tick: reset the deadline and the warning clock.
    if this_tick_nanos <= 0 {
        let reset = TickSchedule {
            next_tick_nanos: now_nanos,
            last_overload_warning_nanos: now_nanos,
        };
        return (reset, None);
    }

    let behind_nanos = now_nanos - next_tick_nanos;
    // Vanilla measures from the scheduled tick, not from now.
    let since_warning = next_tick_nanos - last_overload_warning_nanos;

    if behind_nanos > OVERLOADED_THRESHOLD_NANOS + OVERLOADED_TICKS_THRESHOLD * this_tick_nanos
        && since_warning
            >= OVERLOADED_WARNING_INTERVAL_NANOS
                + OVERLOADED_TICKS_WARNING_INTERVAL * this_tick_nanos
    {
        let ticks = behind_nanos / this_tick_nanos;
        let jumped = next_tick_nanos + ticks * this_tick_nanos;
        let behind_ms = behind_nanos / NANOSECONDS_PER_MILLISECOND;
        let skipped = TickSchedule {
            next_tick_nanos: jumped,
            last_overload_warning_nanos: jumped,
        };
        return (skipped, Some((behind_ms, ticks)));
    }

    (schedule, None)
}

/// Polls world deadlines and runs console commands at a coordinated boundary.
fn wait_until_next_tick(
    server: &Arc<Server>,
    base: Instant,
    next_tick_nanos: i64,
    mut commands: Option<&mut mpsc::Receiver<ConsoleCommand>>,
    worlds: &mut WorldScheduler,
    independent_period: Option<Duration>,
) {
    loop {
        if server.stop_token.is_cancelled() {
            return;
        }

        let remaining = next_tick_nanos - elapsed_nanos(base);
        if remaining <= 0 {
            return;
        }

        if let Some(commands) = commands.as_deref_mut()
            && let Ok(command) = commands.try_recv()
        {
            worlds.wait_idle();
            command.execute(server);
            continue;
        }

        let mut wait = Duration::from_nanos(remaining as u64);
        if let Some(period) = independent_period
            && !server.tick_rate_manager.is_sprinting()
            && !server.tick_rate_manager.is_stepping_forward()
            && !server.storage_checkpoint.manual_capture_requested()
        {
            server
                .storage_checkpoint
                .capture_at_tick_boundary(server, false);
            worlds.dispatch_transfers(server);
            if let Some(world_wait) = worlds.poll(server, period) {
                wait = wait.min(world_wait);
            }
        }
        std::thread::park_timeout(wait);
    }
}

fn elapsed_nanos(base: Instant) -> i64 {
    base.elapsed().as_nanos() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK_20_TPS: i64 = NANOSECONDS_PER_SECOND / 20;

    fn warned_at(nanos: i64) -> TickSchedule {
        TickSchedule {
            next_tick_nanos: 0,
            last_overload_warning_nanos: nanos,
        }
    }

    #[test]
    fn on_time_tick_does_not_skip() {
        let (schedule, skipped) = apply_overload_skip(TickSchedule::new(), 0, TICK_20_TPS);
        assert!(skipped.is_none());
        assert_eq!(schedule.next_tick_nanos, 0);
        assert_eq!(schedule.last_overload_warning_nanos, NEVER_WARNED);
    }

    #[test]
    fn short_lag_catch_up_does_not_skip() {
        let now = 60 * NANOSECONDS_PER_MILLISECOND;
        let (schedule, skipped) = apply_overload_skip(TickSchedule::new(), now, TICK_20_TPS);
        assert!(skipped.is_none());
        assert_eq!(schedule.next_tick_nanos, 0);
    }

    #[test]
    fn first_overload_warns_and_jumps_the_deadline() {
        let now = 3 * NANOSECONDS_PER_SECOND;
        let (schedule, skipped) = apply_overload_skip(TickSchedule::new(), now, TICK_20_TPS);
        let (behind_ms, ticks) = skipped.expect("the first overload always warns");
        assert_eq!((behind_ms, ticks), (3000, 60));
        assert_eq!(schedule.next_tick_nanos, 60 * TICK_20_TPS);
        assert_eq!(
            schedule.last_overload_warning_nanos,
            schedule.next_tick_nanos
        );
    }

    #[test]
    fn overload_warning_is_rate_limited() {
        let now = 3 * NANOSECONDS_PER_SECOND;
        let (schedule, skipped) = apply_overload_skip(warned_at(0), now, TICK_20_TPS);
        assert!(skipped.is_none());
        assert_eq!(schedule.last_overload_warning_nanos, 0);
        // Vanilla holds the deadline too, so the server catches up instead of skipping.
        assert_eq!(schedule.next_tick_nanos, 0);
    }

    #[test]
    fn sprint_resets_deadline_to_now() {
        let now = NANOSECONDS_PER_SECOND;
        let (schedule, skipped) = apply_overload_skip(TickSchedule::new(), now, 0);
        assert!(skipped.is_none());
        assert_eq!(schedule.next_tick_nanos, now);
        assert_eq!(schedule.last_overload_warning_nanos, now);
    }

    /// The deadline advances by one tick per loop while the server keeps up.
    #[test]
    fn deadline_advances_one_tick_per_loop() {
        let mut schedule = TickSchedule::new();

        for tick in 0..100i64 {
            let now = tick * TICK_20_TPS;
            let (updated, skipped) = apply_overload_skip(schedule, now, TICK_20_TPS);
            assert!(skipped.is_none());
            schedule = updated;
            schedule.next_tick_nanos += TICK_20_TPS;
        }

        assert_eq!(schedule.next_tick_nanos, 100 * TICK_20_TPS);
        assert_eq!(schedule.last_overload_warning_nanos, NEVER_WARNED);
    }
}
