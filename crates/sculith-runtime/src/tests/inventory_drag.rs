use super::*;

#[tokio::test]
async fn drag_publishes_one_final_inventory_batch_and_no_selection_previews() {
    use sculith_game::inventory::{Click, DragMode};
    let (stop, receiver) = watch::channel(false);
    let (world, task) = create(receiver);
    let task = tokio::spawn(task);
    let actor = world
        .reserve(profile("drag-actor"), Permissions { can_build: true })
        .await
        .unwrap();
    let id = actor.player.id;
    world.activate(id).await.unwrap();
    world
        .creative_slot(id, 9, Some(BlockKind::Grass), 11)
        .await
        .unwrap();
    world
        .inventory_click(
            id,
            Click::Pickup {
                slot: 9,
                right: false,
            },
        )
        .await
        .unwrap();
    world.configure_observations(crate::observations::INVENTORY | crate::observations::EQUIPMENT);
    let mut hooks = world.subscribe_game();
    let before = world.player_snapshot(id).await.unwrap();
    let revision = world.capture().await.unwrap().revision();
    for click in [
        Click::DragStart {
            mode: DragMode::Even,
        },
        Click::DragAdd {
            slot: 0,
            mode: DragMode::Even,
        },
        Click::DragAdd {
            slot: 1,
            mode: DragMode::Even,
        },
        Click::DragAdd {
            slot: 2,
            mode: DragMode::Even,
        },
    ] {
        assert!(
            world
                .inventory_click(id, click)
                .await
                .unwrap()
                .response
                .result
                .unwrap()
                .is_empty()
        );
        assert_eq!(world.player_snapshot(id).await.unwrap(), before);
        assert!(hooks.try_recv().is_err());
    }
    let completed = world
        .inventory_click(
            id,
            Click::DragFinish {
                mode: DragMode::Even,
            },
        )
        .await
        .unwrap();
    assert_eq!(completed.player.cursor.unwrap().count, 2);
    assert_eq!(completed.player.held().unwrap().count, 3);
    assert!(
        matches!(hooks.recv().await.unwrap().as_ref(), WorldEvent::InventoryChanged { changes,.. } if changes.len()==4)
    );
    assert!(
        matches!(hooks.recv().await.unwrap().as_ref(), WorldEvent::Equipped(p) if p.held().unwrap().count==3)
    );
    assert!(hooks.try_recv().is_err());
    assert!(
        world
            .inventory_click(
                id,
                Click::DragFinish {
                    mode: DragMode::Even
                }
            )
            .await
            .unwrap()
            .response
            .result
            .unwrap()
            .is_empty()
    );
    assert!(hooks.try_recv().is_err());
    assert_eq!(world.capture().await.unwrap().revision(), revision);
    drop(actor);
    stop.send_replace(true);
    task.await.unwrap().unwrap();
}
