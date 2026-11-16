//! Real bounded owner requests, not game ticks or parallel region simulation.
use sculith_runtime::WorldHandle;
use sculith_world::ChunkPos;
use std::{sync::Arc, time::Instant};
use tokio::sync::{Barrier, watch};

#[cfg(target_os = "linux")]
fn cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", std::process::id())).ok()?;
    let parts = stat
        .rsplit_once(") ")?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    parts
        .get(11)?
        .parse::<u64>()
        .ok()?
        .checked_add(parts.get(12)?.parse().ok()?)
}
#[cfg(not(target_os = "linux"))]
fn cpu_ticks() -> Option<u64> {
    None
}

async fn request(world: &WorldHandle, capture: bool, index: usize) {
    if capture {
        assert_eq!(world.capture().await.unwrap().revision(), 0);
    } else {
        assert_eq!(
            world
                .snapshot(ChunkPos {
                    x: (index % 4) as i32 - 2,
                    z: (index / 4 % 4) as i32 - 2
                })
                .await
                .unwrap()
                .revision(),
            0
        );
    }
}

#[tokio::main(worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let iterations = std::env::args()
        .nth(1)
        .unwrap_or("20000".into())
        .parse::<usize>()?;
    anyhow::ensure!(
        (1024..=2_000_000).contains(&iterations) && iterations.is_multiple_of(32),
        "iterations must be 1024..2000000 and divisible by 32"
    );
    for (case, clients, capture) in [
        ("snapshot-one", 1, false),
        ("snapshot-thirty-two", 32, false),
        ("capture-one", 1, true),
        ("capture-thirty-two", 32, true),
    ] {
        let (stop, shutdown) = watch::channel(false);
        let (world, owner) = sculith_runtime::create(shutdown);
        let owner = tokio::spawn(owner);
        // All cases start with the same sixteen resident chunks and revision.
        for index in 0..128 {
            request(&world, false, index).await;
        }
        for index in 0..128 {
            request(&world, capture, index).await;
        }
        let ready = Arc::new(Barrier::new(clients + 1));
        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..clients {
            let world = world.clone();
            let ready = ready.clone();
            tasks.spawn(async move {
                let mut samples = Vec::with_capacity(iterations / clients);
                ready.wait().await;
                for _ in 0..iterations / clients {
                    let started = Instant::now();
                    request(&world, capture, index).await;
                    samples.push(started.elapsed().as_nanos() as u64);
                }
                samples
            });
        }
        println!(
            "{{\"kind\":\"phase_start\",\"case\":\"{case}\",\"host_pid\":{},\"worker_pids\":[]}}",
            std::process::id()
        );
        let before = cpu_ticks();
        let started = Instant::now();
        ready.wait().await;
        let mut latencies = Vec::with_capacity(iterations);
        while let Some(samples) = tasks.join_next().await {
            latencies.extend(samples?);
        }
        let wall = started.elapsed().as_nanos();
        let cpu = cpu_ticks().zip(before).map(|(a, b)| a.saturating_sub(b));
        assert_eq!(latencies.len(), iterations);
        latencies.sort_unstable();
        let q = |percent: usize| latencies[(latencies.len() * percent).div_ceil(100) - 1];
        println!(
            "{{\"kind\":\"phase_end\",\"case\":\"{case}\",\"iterations\":{iterations},\"clients\":{clients},\"workers\":0,\"subscribers\":0,\"mode\":\"off\",\"delivered_requests\":{iterations},\"wall_ns\":{wall},\"p50_ns\":{},\"p95_ns\":{},\"p99_ns\":{},\"host_cpu_ticks\":{},\"worker_cpu_ticks\":0}}",
            q(50),
            q(95),
            q(99),
            cpu.map_or("null".into(), |v| v.to_string())
        );
        stop.send_replace(true);
        owner.await??;
    }
    Ok(())
}
