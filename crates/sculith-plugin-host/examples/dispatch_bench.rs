//! Real Rust child-process IPC; no world simulation or network throughput claim.
use sculith_core::ServerStatus;
use sculith_plugin_api::{BlockChange, Event, WorldActor};
use sculith_plugin_host::{PluginConfig, Plugins};
use serde_json::json;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
fn cpu_ticks(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = stat
        .rsplit_once(") ")?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    tail.get(11)?
        .parse::<u64>()
        .ok()?
        .checked_add(tail.get(12)?.parse::<u64>().ok()?)
}
#[cfg(not(target_os = "linux"))]
fn cpu_ticks(_: u32) -> Option<u64> {
    None
}
fn child_ticks(pids: &[u32]) -> Option<u64> {
    pids.iter()
        .try_fold(0u64, |sum, pid| sum.checked_add(cpu_ticks(*pid)?))
}
fn event(kind: &str, revision: u64) -> Event {
    if kind == "tick" {
        Event::Tick { tick: revision }
    } else {
        Event::WorldBlocksChanged {
            region_id: 0,
            previous_revision: revision,
            revision: revision + 1,
            actor: WorldActor::Plugin {
                name: "benchmark".into(),
            },
            changes: (0..64)
                .map(|x| BlockChange {
                    x,
                    y: 4,
                    z: 1,
                    before: "minecraft:air".into(),
                    after: "minecraft:stone".into(),
                })
                .collect(),
        }
    }
}

#[tokio::main(worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let fixture = PathBuf::from(args.next().expect("Rust fixture path")).canonicalize()?;
    let iterations = args.next().unwrap_or("2000".into()).parse::<usize>()?;
    anyhow::ensure!(
        (128..=20_000).contains(&iterations),
        "iterations must be 128..20000"
    );
    let mode = args.next().unwrap_or("off".into());
    anyhow::ensure!(
        ["off", "monitor", "profile"].contains(&mode.as_str()),
        "unknown profiler mode"
    );
    for (name, count, subscribed, kind) in [
        ("world-one-of-one", 1, 1, "world"),
        ("world-one-of-eight", 8, 1, "world"),
        ("world-one-of-thirty-two", 32, 1, "world"),
        ("world-all-thirty-two", 32, 32, "world"),
        ("world-none-of-thirty-two", 32, 0, "world"),
        ("tick-one-of-thirty-two", 32, 1, "tick"),
    ] {
        let mut status = ServerStatus::new("benchmark".into(), 20).map_err(anyhow::Error::msg)?;
        let configs = (0..count)
            .map(|index| PluginConfig {
                path: fixture.clone(),
                args: vec![
                    format!("bench-{index}"),
                    if index < subscribed { kind } else { "none" }.into(),
                ],
                timeout_ms: 10000,
            })
            .collect::<Vec<_>>();
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let (world, task) = sculith_runtime::create(shutdown);
        let task = tokio::spawn(task);
        let mut plugins = Plugins::load(&configs, &status).await?;
        match mode.as_str() {
            "monitor" => plugins.monitor(true),
            "profile" => plugins.start_profile(Duration::from_secs(300))?,
            _ => {}
        }
        for index in 0..128 {
            plugins
                .dispatch(event(kind, index), &mut status, &world)
                .await;
        }
        let events = (0..iterations)
            .map(|index| event(kind, index as u64 + 128))
            .collect::<Vec<_>>();
        let initial = plugins.profile_snapshot();
        let pids = initial
            .plugins
            .iter()
            .map(|p| p.worker_pid.expect("fixture alive"))
            .collect::<Vec<_>>();
        println!(
            "{}",
            json!({"kind":"phase_start","case":name,"host_pid":std::process::id(),"worker_pids":pids})
        );
        let host_before = cpu_ticks(std::process::id());
        let children_before = child_ticks(&pids);
        let mut latencies = Vec::with_capacity(iterations);
        let started = Instant::now();
        for event in events {
            let at = Instant::now();
            plugins.dispatch(event, &mut status, &world).await;
            latencies.push(at.elapsed().as_nanos() as u64);
        }
        let elapsed = started.elapsed();
        let host_after = cpu_ticks(std::process::id());
        let children_after = child_ticks(&pids);
        latencies.sort_unstable();
        let quantile = |p: usize| latencies[(latencies.len() * p).div_ceil(100) - 1];
        let profile = plugins.profile_snapshot();
        for (index, worker) in profile.plugins.iter().enumerate() {
            anyhow::ensure!(!worker.disabled, "fixture disabled during benchmark");
            let expected = if index < subscribed {
                iterations as u64 + 129
            } else {
                1
            };
            anyhow::ensure!(
                worker.ipc.calls == expected && worker.ipc.failures == 0,
                "benchmark delivery count mismatch"
            );
        }
        println!(
            "{}",
            json!({"kind":"phase_end","case":name,"iterations":iterations,"workers":count,"subscribers":subscribed,"mode":mode,
            "wall_ns":elapsed.as_nanos(),"p50_ns":quantile(50),"p95_ns":quantile(95),"p99_ns":quantile(99),
            "host_cpu_ticks":host_after.zip(host_before).map(|(a,b)|a.saturating_sub(b)),
            "worker_cpu_ticks":children_after.zip(children_before).map(|(a,b)|a.saturating_sub(b)),
            "delivered_requests":subscribed*iterations,"profile":profile})
        );
        plugins.stop(&mut status, &world).await;
        stop.send_replace(true);
        task.await??;
    }
    Ok(())
}
