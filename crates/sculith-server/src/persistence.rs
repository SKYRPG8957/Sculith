//! Serial checkpoints of immutable owner captures; no filesystem work on the owner.

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use sculith_runtime::WorldHandle;
use sculith_world::RegionSnapshot;
use tokio::{
    sync::{Notify, watch},
    time::MissedTickBehavior,
};
use tracing::info;

/// One active capture/write at most. Shutdown waits for an in-flight write;
/// callers must join this worker before capturing and saving the final state.
pub async fn run(
    world: WorldHandle,
    path: Option<PathBuf>,
    period: Duration,
    mut saved_revision: Option<u64>,
    mut shutdown: watch::Receiver<bool>,
    manual: Arc<Notify>,
) -> Result<()> {
    if *shutdown.borrow() {
        return Ok(());
    }
    let Some(path) = path else {
        let _ = shutdown.changed().await;
        return Ok(());
    };
    let mut timer = (!period.is_zero()).then(|| {
        let mut timer = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
        timer
    });
    loop {
        let requested = tokio::select! {
            biased;
            _ = shutdown.changed() => return Ok(()),
            _ = manual.notified() => true,
            _ = async {
                match timer.as_mut() {
                    Some(timer) => { timer.tick().await; }
                    None => std::future::pending::<()>().await,
                }
            } => false,
        };
        let snapshot = world.capture().await.context("checkpoint capture failed")?;
        let revision = snapshot.revision();
        if saved_revision == Some(revision) {
            if requested {
                info!(revision, "World already saved");
            }
            continue;
        }
        save(path.clone(), snapshot)
            .await
            .context("autosave failed")?;
        saved_revision = Some(revision);
        info!(revision, "World saved");
    }
}

pub async fn save(path: PathBuf, snapshot: RegionSnapshot) -> Result<()> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        sculith_world::storage::save_snapshot(&path, &snapshot)?;
        Ok(())
    })
    .await
    .context("snapshot saving worker failed")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use sculith_core::world::{WorldBlock, WorldOperation, WorldPosition};
    use sculith_world::{Region, storage};

    fn owner() -> (
        WorldHandle,
        watch::Sender<bool>,
        tokio::task::JoinHandle<Result<()>>,
    ) {
        let (stop, shutdown) = watch::channel(false);
        let (world, run) =
            sculith_runtime::with_region(Region::flat_origin(), 1, shutdown).unwrap();
        (world, stop, tokio::spawn(run))
    }

    async fn edit(world: &WorldHandle, revision: u64) {
        world
            .plugin_world(
                "save-fixture".into(),
                revision as u32,
                WorldOperation::Edit {
                    region_id: 0,
                    expected_revision: revision,
                    blocks: vec![WorldBlock {
                        position: WorldPosition { x: 0, y: 4, z: 0 },
                        block: if revision.is_multiple_of(2) {
                            "minecraft:stone"
                        } else {
                            "minecraft:air"
                        }
                        .into(),
                    }],
                },
            )
            .await
            .unwrap();
        // A capture is queued after the edit on the same owner mailbox.
        assert_eq!(world.capture().await.unwrap().revision(), revision + 1);
    }

    async fn saved(path: &std::path::Path, revision: u64) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if storage::load(path).is_ok_and(|region| region.revision() == revision) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn checkpoints_live_owner_edits_and_final_save_cannot_be_overtaken() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/region.sculith");
        let (world, stop_owner, owner_task) = owner();
        let (stop, shutdown) = watch::channel(false);
        let task = tokio::spawn(run(
            world.clone(),
            Some(path.clone()),
            Duration::from_millis(5),
            None,
            shutdown,
            Arc::new(Notify::new()),
        ));
        saved(&path, 0).await;
        edit(&world, 0).await;
        saved(&path, 1).await;
        assert!(!owner_task.is_finished());
        stop.send_replace(true);
        task.await.unwrap().unwrap();
        edit(&world, 1).await;
        save(path.clone(), world.capture().await.unwrap())
            .await
            .unwrap();
        stop_owner.send_replace(true);
        owner_task.await.unwrap().unwrap();
        assert_eq!(storage::load(path).unwrap().revision(), 2);
    }

    #[tokio::test]
    async fn unchanged_world_is_skipped_and_failed_checkpoint_preserves_foreign_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("region.sculith");
        storage::save(&path, &Region::flat_origin()).unwrap();
        let (world, stop_owner, owner_task) = owner();
        let (_stop, shutdown) = watch::channel(false);
        let task = tokio::spawn(run(
            world.clone(),
            Some(path.clone()),
            Duration::from_millis(5),
            Some(0),
            shutdown,
            Arc::new(Notify::new()),
        ));
        let foreign = b"This file must not be replaced.";
        std::fs::write(&path, foreign).unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(
            !task.is_finished(),
            "unchanged revisions must not access disk"
        );
        edit(&world, 0).await;
        let error = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(format!("{error:#}").contains("autosave failed"));
        assert_eq!(std::fs::read(path).unwrap(), foreign);
        stop_owner.send_replace(true);
        owner_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn disabled_checkpoint_still_allows_a_final_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("region.sculith");
        let (world, stop_owner, owner_task) = owner();
        let (stop, shutdown) = watch::channel(false);
        let task = tokio::spawn(run(
            world.clone(),
            Some(path.clone()),
            Duration::ZERO,
            None,
            shutdown,
            Arc::new(Notify::new()),
        ));
        edit(&world, 0).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!path.exists());
        stop.send_replace(true);
        task.await.unwrap().unwrap();
        save(path.clone(), world.capture().await.unwrap())
            .await
            .unwrap();
        assert_eq!(storage::load(path).unwrap().revision(), 1);
        stop_owner.send_replace(true);
        owner_task.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn manual_saves_work_without_a_timer_and_coalesce_pending_requests() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("region.sculith");
        let (world, stop_owner, owner_task) = owner();
        let (stop, shutdown) = watch::channel(false);
        let manual = Arc::new(Notify::new());
        for _ in 0..1000 {
            manual.notify_one();
        }
        let task = tokio::spawn(run(
            world.clone(),
            Some(path.clone()),
            Duration::ZERO,
            None,
            shutdown,
            manual.clone(),
        ));
        saved(&path, 0).await;
        assert_eq!(world.profile_snapshot().completed_commands, 1);
        edit(&world, 0).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(storage::load(&path).unwrap().revision(), 0);
        manual.notify_one();
        saved(&path, 1).await;
        stop.send_replace(true);
        task.await.unwrap().unwrap();
        stop_owner.send_replace(true);
        owner_task.await.unwrap().unwrap();
    }
}
