use super::*;
use std::time::Duration;

#[tokio::test]
async fn profiles_backpressure_queue_and_work_without_changing_cancellation_or_bounds() {
    let (stop, shutdown) = watch::channel(false);
    let (world, owner) = create(shutdown);
    world.start_profile(Duration::from_secs(30)).unwrap();
    let mut queued = Vec::new();
    for _ in 0..32 {
        let world = world.clone();
        queued.push(tokio::spawn(async move {
            world.snapshot(ChunkPos { x: 0, z: 0 }).await
        }));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while world.commands.capacity() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (began, started) = oneshot::channel();
    let delayed = {
        let world = world.clone();
        tokio::spawn(async move {
            let _ = began.send(());
            world.snapshot(ChunkPos { x: 1, z: 0 }).await
        })
    };
    started.await.unwrap();
    let cancelled = {
        let world = world.clone();
        tokio::spawn(async move { world.capture().await })
    };
    tokio::task::yield_now().await;
    cancelled.abort();
    assert!(cancelled.await.unwrap_err().is_cancelled());
    // An already-admitted command still executes after its caller disappears.
    let abandoned = queued.pop().unwrap();
    abandoned.abort();
    assert!(abandoned.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!delayed.is_finished());
    assert_eq!(world.profile_snapshot().occupied_capacity, 32);
    let owner = tokio::spawn(owner);
    for task in queued {
        assert_eq!(task.await.unwrap().unwrap().revision(), 0);
    }
    delayed.await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while world.profile_snapshot().completed_commands < 33 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let snapshot = world.profile_snapshot();
    assert_eq!(snapshot.queue_capacity, 32);
    assert_eq!(
        (
            snapshot.completed_commands,
            snapshot.totals.calls,
            snapshot.aborted_commands
        ),
        (33, 33, 0)
    );
    assert!(snapshot.totals.maximum.admission_ns >= 10_000_000);
    assert!(snapshot.totals.maximum.queue_ns >= 10_000_000);
    assert!(snapshot.samples.iter().all(|sample| {
        let t = sample.times;
        !sample.aborted
            && sample.remaining_queue_depth <= 32
            && t.total_ns == t.admission_ns + t.queue_ns + t.service_ns
    }));
    // Ordinary game rejections are completed handlers, not owner aborts.
    assert!(world.player_snapshot(0).await.is_err());
    tokio::time::timeout(Duration::from_secs(2), async {
        while world.profile_snapshot().completed_commands < 34 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(world.profile_snapshot().aborted_commands, 0);
    world.monitor(false).unwrap();
    assert!(world.profile_snapshot().samples.is_empty());
    assert_eq!(world.capture().await.unwrap().revision(), 0);
    stop.send_replace(true);
    owner.await.unwrap().unwrap();
    assert!(world.profile_snapshot().owner_closed);
    assert!(world.capture().await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_reconfiguration_cannot_cancel_game_work_or_reset_lifetime_counts() {
    let (stop, shutdown) = watch::channel(false);
    let (world, owner) = create(shutdown);
    let owner = tokio::spawn(owner);
    let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
    let controller = {
        let world = world.clone();
        let gate = gate.clone();
        std::thread::spawn(move || {
            gate.wait();
            for _ in 0..500 {
                world.monitor(true).unwrap();
                world.start_profile(Duration::from_secs(1)).unwrap();
                world.monitor(false).unwrap();
            }
        })
    };
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..4 {
        let world = world.clone();
        tasks.spawn(async move {
            for _ in 0..1000 {
                assert_eq!(
                    world
                        .snapshot(ChunkPos { x: index, z: 0 })
                        .await
                        .unwrap()
                        .revision(),
                    0
                );
            }
        });
    }
    gate.wait();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
    })
    .await
    .unwrap();
    controller.join().unwrap();
    world.start_profile(Duration::from_secs(30)).unwrap();
    for _ in 0..11 {
        world.capture().await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while world.profile_snapshot().completed_commands < 4011 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let snapshot = world.profile_snapshot();
    assert_eq!(snapshot.aborted_commands, 0);
    assert_eq!(snapshot.completed_commands, 4011);
    assert_eq!(snapshot.totals.calls, 11);
    assert!(
        snapshot
            .samples
            .iter()
            .all(|s| s.start_ns < 30_000_000_000 && !s.aborted)
    );
    stop.send_replace(true);
    owner.await.unwrap().unwrap();
}
