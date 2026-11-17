use super::*;

#[test]
#[ignore = "manual drag gesture kernel/layout measurement"]
fn drag_gesture_measurement() {
    use std::{hint::black_box, mem::size_of, time::Instant};
    for slots in [3, 36] {
        let start = Instant::now();
        for _ in 0..10_000 {
            let mut inventory = cursor(64);
            inventory
                .click(
                    Click::DragStart {
                        mode: DragMode::Even,
                    },
                    true,
                )
                .unwrap();
            for slot in 0..slots {
                inventory
                    .click(
                        Click::DragAdd {
                            slot,
                            mode: DragMode::Even,
                        },
                        true,
                    )
                    .unwrap();
            }
            black_box(
                inventory
                    .click(
                        Click::DragFinish {
                            mode: DragMode::Even,
                        },
                        true,
                    )
                    .unwrap(),
            );
            black_box(inventory);
        }
        println!(
            "drag_gesture slots={slots} gestures=10000 wall_ns={} inventory_bytes={} drag_bytes={}",
            start.elapsed().as_nanos(),
            size_of::<Inventory>(),
            size_of::<Option<drag::Drag>>()
        );
    }
}

fn cursor(count: u8) -> Inventory {
    let mut inventory = Inventory::default();
    inventory.give(BlockKind::Grass, u16::from(count)).unwrap();
    inventory
        .click(
            Click::Pickup {
                slot: 0,
                right: false,
            },
            true,
        )
        .unwrap();
    inventory
}

fn begin(inventory: &mut Inventory, mode: DragMode, slots: &[u8]) {
    let before = inventory.state.clone();
    assert!(
        inventory
            .click(Click::DragStart { mode }, true)
            .unwrap()
            .is_empty()
    );
    for &slot in slots {
        assert!(
            inventory
                .click(Click::DragAdd { slot, mode }, true)
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(inventory.state, before);
}

fn finish(inventory: &mut Inventory, mode: DragMode) -> Vec<Change> {
    inventory.click(Click::DragFinish { mode }, true).unwrap()
}

#[test]
fn even_drag_commits_once_preserves_remainder_identity_and_request_history() {
    let mut inventory = cursor(11);
    let original = inventory.cursor().unwrap();
    begin(&mut inventory, DragMode::Even, &[0, 1, 9, 1]);
    assert_eq!(finish(&mut inventory, DragMode::Even).len(), 4);
    for slot in [0, 1, 9] {
        assert_eq!(inventory.slots()[slot].unwrap().count, 3);
    }
    assert_eq!(
        inventory.cursor().unwrap(),
        ItemStack {
            count: 2,
            ..original
        }
    );
    let ids: Vec<_> = inventory
        .slots()
        .iter()
        .flatten()
        .map(|s| s.net_id)
        .chain(inventory.cursor().map(|s| s.net_id))
        .collect();
    assert_eq!(
        ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
        4
    );
    assert!(inventory.history.is_empty() && inventory.lowest_request_id.is_none());
    let after = inventory.clone();
    assert!(finish(&mut inventory, DragMode::Even).is_empty());
    assert_eq!(inventory, after);
}

#[test]
fn distribution_counts_full_matching_slots_and_filters_incompatible_targets() {
    for mode in [DragMode::Even, DragMode::OneEach] {
        let mut inventory = cursor(11);
        inventory
            .replace_creative(1, Some(BlockKind::Grass), 64, true)
            .unwrap();
        inventory
            .replace_creative(2, Some(BlockKind::Stone), 1, true)
            .unwrap();
        inventory
            .replace_creative(3, Some(BlockKind::Grass), 63, true)
            .unwrap();
        let full = inventory.slots()[1];
        let almost = inventory.slots()[3].unwrap();
        begin(&mut inventory, mode, &[0, 1, 2, 3]);
        finish(&mut inventory, mode);
        let share = if mode == DragMode::Even { 3 } else { 1 };
        assert_eq!(inventory.slots()[0].unwrap().count, share);
        assert_eq!(inventory.slots()[1], full);
        assert_eq!(inventory.slots()[2].unwrap().kind, BlockKind::Stone);
        assert_eq!(
            inventory.slots()[3].unwrap(),
            ItemStack {
                count: 64,
                ..almost
            }
        );
        assert_eq!(inventory.cursor().unwrap().count, 10 - share);
    }
}

#[test]
fn single_target_and_full_grid_follow_cursor_capacity_without_duplicate_weight() {
    for mode in [DragMode::Even, DragMode::OneEach] {
        let mut inventory = cursor(11);
        inventory
            .replace_creative(0, Some(BlockKind::Grass), 60, true)
            .unwrap();
        begin(&mut inventory, mode, &[0, 0]);
        finish(&mut inventory, mode);
        let moved = if mode == DragMode::Even { 4 } else { 1 };
        assert_eq!(inventory.cursor().unwrap().count, 11 - moved);
        assert_eq!(inventory.slots()[0].unwrap().count, 60 + moved);
        let mut inventory = cursor(64);
        begin(&mut inventory, mode, &(0..36).collect::<Vec<_>>());
        assert_eq!(finish(&mut inventory, mode).len(), 37);
        assert!(
            inventory
                .slots()
                .iter()
                .all(|s| s.is_some_and(|s| s.count == 1))
        );
        assert_eq!(inventory.cursor().unwrap().count, 28);
    }
    let mut inventory = cursor(1);
    begin(&mut inventory, DragMode::OneEach, &[0, 1]);
    finish(&mut inventory, DragMode::OneEach);
    assert!(inventory.slots()[1].is_none() && inventory.cursor().is_none());
}

#[test]
fn malformed_interrupted_or_changed_gestures_cannot_commit_items() {
    let mut inventory = cursor(11);
    let before = inventory.state.clone();
    assert!(finish(&mut inventory, DragMode::Even).is_empty());
    begin(&mut inventory, DragMode::Even, &[0, 1]);
    assert!(finish(&mut inventory, DragMode::OneEach).is_empty());
    assert_eq!(inventory.state, before);
    begin(&mut inventory, DragMode::Even, &[0, 1]);
    assert!(
        inventory
            .click(
                Click::Pickup {
                    slot: 0,
                    right: false
                },
                true
            )
            .unwrap()
            .is_empty()
    );
    assert_eq!(inventory.state, before);
    assert!(finish(&mut inventory, DragMode::Even).is_empty());
    begin(&mut inventory, DragMode::Even, &[0, 1]);
    inventory.click(Click::CancelDrag, true).unwrap();
    assert!(finish(&mut inventory, DragMode::Even).is_empty());
    begin(&mut inventory, DragMode::Even, &[0, 1]);
    assert!(
        inventory
            .click(
                Click::DragAdd {
                    slot: 36,
                    mode: DragMode::Even
                },
                true
            )
            .is_err()
    );
    assert!(finish(&mut inventory, DragMode::Even).is_empty());
    assert_eq!(inventory.state, before);
}

#[test]
fn cursor_change_server_grant_and_permission_loss_invalidate_active_drag() {
    let mut inventory = cursor(11);
    begin(&mut inventory, DragMode::Even, &[0, 1]);
    inventory.state.cursor.as_mut().unwrap().count -= 1;
    let before = inventory.state.clone();
    assert!(finish(&mut inventory, DragMode::Even).is_empty());
    assert_eq!(inventory.state, before);
    begin(&mut inventory, DragMode::Even, &[0, 1]);
    inventory.give(BlockKind::Stone, 1).unwrap();
    let before = inventory.state.clone();
    assert!(finish(&mut inventory, DragMode::Even).is_empty());
    assert_eq!(inventory.state, before);
    begin(&mut inventory, DragMode::Even, &[1, 2]);
    assert!(
        inventory
            .click(
                Click::DragFinish {
                    mode: DragMode::Even
                },
                false
            )
            .is_err()
    );
    assert!(finish(&mut inventory, DragMode::Even).is_empty());
    assert_eq!(inventory.state, before);
}

#[test]
fn drag_rolls_back_all_splits_and_id_allocations_when_a_late_allocation_fails() {
    let mut inventory = cursor(10);
    begin(&mut inventory, DragMode::Even, &[0, 1, 2]);
    inventory.state.next_stack_id = i32::MAX as u64 - 1;
    let before = inventory.state.clone();
    assert!(
        inventory
            .click(
                Click::DragFinish {
                    mode: DragMode::Even
                },
                true
            )
            .is_err()
    );
    assert_eq!(inventory.state, before);
    assert!(finish(&mut inventory, DragMode::Even).is_empty());
    assert_eq!(inventory.state, before);
}
