use super::*;
use sculith_core::world::{WorldBlock, WorldError, WorldOperation, WorldPosition, WorldResult};
use std::time::Duration;

fn position(x: i32) -> WorldPosition {
    WorldPosition { x, y: 4, z: 1 }
}
fn edit(revision: u64, x: i32, name: &str) -> WorldOperation {
    WorldOperation::Edit {
        region_id: 0,
        expected_revision: revision,
        blocks: vec![WorldBlock {
            position: position(x),
            block: name.into(),
        }],
    }
}
async fn result(
    receiver: &mut broadcast::Receiver<Arc<WorldEvent>>,
    plugin: &str,
    request: u32,
) -> (WorldResult, Vec<Arc<WorldEvent>>) {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut notifications = vec![];
        for _ in 0..64 {
            let event = receiver.recv().await.unwrap();
            if let WorldEvent::PluginWorldResult {
                plugin: owner,
                request_id,
                result,
            } = event.as_ref()
            {
                assert_eq!((owner.as_str(), *request_id), (plugin, request));
                return (result.clone(), notifications);
            }
            notifications.push(event);
        }
        panic!("world result missing")
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn world_queries_are_private_and_plugin_commits_have_no_fabricated_player() {
    let (stop, receiver) = watch::channel(false);
    let (world, owner) = create(receiver);
    let owner = tokio::spawn(owner);
    let mut hooks = world.subscribe_game();
    let mut clients = world.subscribe();
    world
        .plugin_world(
            "builder".into(),
            1,
            WorldOperation::Read {
                positions: vec![position(1)],
            },
        )
        .await
        .unwrap();
    let (read, notifications) = result(&mut hooks, "builder", 1).await;
    assert!(notifications.is_empty() && clients.try_recv().is_err());
    assert!(
        matches!(read,WorldResult::Snapshot {region,blocks} if region.id==0 && region.revision==0 && blocks[0].block=="minecraft:air")
    );
    assert!(world.capture().await.unwrap().chunks().is_empty());
    world
        .plugin_world(
            "builder".into(),
            2,
            WorldOperation::Edit {
                region_id: 0,
                expected_revision: 0,
                blocks: vec![
                    WorldBlock {
                        position: position(-1),
                        block: "minecraft:stone".into(),
                    },
                    WorldBlock {
                        position: position(17),
                        block: "minecraft:dirt".into(),
                    },
                ],
            },
        )
        .await
        .unwrap();
    let (updated, notifications) = result(&mut hooks, "builder", 2).await;
    assert!(
        matches!(updated,WorldResult::Updated {region,changes,..} if region.revision==1 && changes.len()==2)
    );
    assert_eq!(notifications.len(), 1);
    let broadcast = clients.recv().await.unwrap();
    assert!(Arc::ptr_eq(&broadcast, &notifications[0]));
    let WorldEvent::PluginBlocks {
        plugin,
        batch,
        revisions,
    } = broadcast.as_ref()
    else {
        panic!("unexpected player attribution")
    };
    assert_eq!(
        (
            plugin.as_str(),
            batch.revision,
            batch.changes.len(),
            revisions.len()
        ),
        ("builder", 1, 2, 2)
    );
    assert!(revisions.values().all(|r| *r == 1));
    assert!(clients.try_recv().is_err());
    world
        .plugin_world(
            "other".into(),
            2,
            WorldOperation::Read {
                positions: vec![position(-1)],
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(result(&mut hooks,"other",2).await.0,WorldResult::Snapshot {blocks,..} if blocks[0].block=="minecraft:stone")
    );
    assert_eq!(world.online_players(), 0);
    stop.send_replace(true);
    owner.await.unwrap().unwrap();
}

#[tokio::test]
async fn rejected_and_noop_world_edits_emit_only_private_results() {
    let (stop, receiver) = watch::channel(false);
    let (world, owner) = create(receiver);
    let owner = tokio::spawn(owner);
    let mut hooks = world.subscribe_game();
    let mut clients = world.subscribe();
    for (id, operation, reason) in [
        (
            1,
            edit(0, i32::MAX, "minecraft:stone"),
            WorldError::OutsideRegion,
        ),
        (
            2,
            edit(0, 1, "minecraft:glass"),
            WorldError::UnsupportedBlock,
        ),
        (
            3,
            edit(99, 1, "minecraft:stone"),
            WorldError::RevisionConflict {
                expected: 99,
                actual: 0,
            },
        ),
    ] {
        world
            .plugin_world("builder".into(), id, operation)
            .await
            .unwrap();
        let (outcome, notifications) = result(&mut hooks, "builder", id).await;
        assert!(matches!(outcome,WorldResult::Rejected {reason:r} if r==reason));
        assert!(notifications.is_empty() && clients.try_recv().is_err());
    }
    world
        .plugin_world("builder".into(), 4, edit(0, 1, "minecraft:air"))
        .await
        .unwrap();
    let (outcome, notifications) = result(&mut hooks, "builder", 4).await;
    assert!(
        matches!(outcome,WorldResult::Updated {changes,region,..} if changes.is_empty() && region.revision==0)
    );
    assert!(notifications.is_empty() && clients.try_recv().is_err());
    assert!(world.capture().await.unwrap().chunks().is_empty());
    assert!(
        world
            .plugin_world(
                "invalid owner".into(),
                5,
                WorldOperation::Read { positions: vec![] }
            )
            .await
            .is_err()
    );
    assert!(
        world
            .plugin_world(
                "builder".into(),
                6,
                WorldOperation::Read {
                    positions: vec![position(1); 65]
                }
            )
            .await
            .is_err()
    );
    assert!(hooks.try_recv().is_err());
    stop.send_replace(true);
    owner.await.unwrap().unwrap();
    assert!(
        world
            .plugin_world(
                "builder".into(),
                7,
                WorldOperation::Read { positions: vec![] }
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn competing_plugin_edits_have_one_cas_winner_in_the_shared_owner() {
    let (stop, receiver) = watch::channel(false);
    let (world, owner) = create(receiver);
    let owner = tokio::spawn(owner);
    let mut hooks = world.subscribe_game();
    let mut clients = world.subscribe();
    let a = world.clone();
    let b = world.clone();
    let (a, b) = tokio::join!(
        a.plugin_world("first".into(), 1, edit(0, 1, "minecraft:stone")),
        b.plugin_world("second".into(), 1, edit(0, 1, "minecraft:dirt"))
    );
    a.unwrap();
    b.unwrap();
    let mut success = 0;
    let mut conflict = 0;
    let mut results = 0;
    while results < 2 {
        if let WorldEvent::PluginWorldResult { result, .. } = hooks.recv().await.unwrap().as_ref() {
            match result {
                WorldResult::Updated { region, .. } => {
                    assert_eq!(region.revision, 1);
                    success += 1;
                }
                WorldResult::Rejected {
                    reason:
                        WorldError::RevisionConflict {
                            expected: 0,
                            actual: 1,
                        },
                } => conflict += 1,
                _ => panic!("unexpected CAS result"),
            }
            results += 1;
        }
    }
    assert_eq!((success, conflict), (1, 1));
    assert!(matches!(
        clients.recv().await.unwrap().as_ref(),
        WorldEvent::PluginBlocks { .. }
    ));
    assert!(clients.try_recv().is_err());
    assert_eq!(world.capture().await.unwrap().revision(), 1);
    stop.send_replace(true);
    owner.await.unwrap().unwrap();
}
