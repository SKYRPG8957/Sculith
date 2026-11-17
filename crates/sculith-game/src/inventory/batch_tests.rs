use super::*;

fn reference(slot: Slot, expected_id: i32) -> SlotRef {
    SlotRef { slot, expected_id }
}

#[test]
fn compound_positive_id_swaps_follow_stacks_without_allocating() {
    let mut inventory = Inventory::default();
    inventory.give(BlockKind::Grass, 36).unwrap();
    inventory.give(BlockKind::Dirt, 1).unwrap();
    for (sequence, count) in [2, 36, 64].into_iter().enumerate() {
        let before = inventory.state.clone();
        let actions = (0..count)
            .map(|offset| Action::Swap {
                source: reference(Slot::Inventory(0), before.slots[offset % 2].unwrap().net_id),
                destination: reference(
                    Slot::Inventory(1),
                    before.slots[1 - offset % 2].unwrap().net_id,
                ),
            })
            .collect();
        assert!(
            inventory
                .apply(
                    &Request {
                        id: -(sequence as i32 + 1),
                        actions
                    },
                    true
                )
                .result
                .is_ok()
        );
        assert_eq!(inventory.state, before);
    }
}

#[test]
fn full_inventory_split_and_merge_conserve_counts_and_stack_identity() {
    let mut inventory = Inventory::default();
    inventory.give(BlockKind::Grass, 36).unwrap();
    let source = reference(Slot::Inventory(0), inventory.slots()[0].unwrap().net_id);
    let actions = std::iter::once(Slot::Cursor)
        .chain((1..36).map(Slot::Inventory))
        .map(|slot| Action::Transfer {
            source,
            destination: reference(slot, 0),
            count: 1,
        })
        .collect();
    let split = Request { id: -1, actions };
    let response = inventory.apply(&split, true);
    assert_eq!(response.result.as_ref().unwrap().len(), 37);
    assert!(inventory.slots()[0].is_none());
    let stacks: Vec<_> = inventory
        .slots()
        .iter()
        .flatten()
        .copied()
        .chain(inventory.cursor())
        .collect();
    assert_eq!(stacks.len(), 36);
    assert!(
        stacks
            .iter()
            .all(|s| s.kind == BlockKind::Grass && s.count == 1)
    );
    assert_eq!(
        stacks
            .iter()
            .map(|s| s.net_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        36
    );
    assert_eq!(inventory.slots()[35].unwrap().net_id, source.expected_id);
    let cursor_id = inventory.cursor().unwrap().net_id;
    let allocation = inventory.state.next_stack_id;
    let actions = (1..36)
        .map(|slot| Action::Transfer {
            source: reference(
                Slot::Inventory(slot),
                inventory.slots()[slot as usize].unwrap().net_id,
            ),
            destination: reference(Slot::Cursor, cursor_id),
            count: 1,
        })
        .collect();
    assert!(
        inventory
            .apply(&Request { id: -2, actions }, true)
            .result
            .is_ok()
    );
    assert!(inventory.slots().iter().all(Option::is_none));
    assert_eq!(inventory.cursor().unwrap().count, 36);
    assert_eq!(inventory.cursor().unwrap().net_id, cursor_id);
    assert_eq!(inventory.state.next_stack_id, allocation);
    let before = inventory.state.clone();
    assert!(inventory.apply(&split, true).replayed);
    assert_eq!(inventory.state, before);
}

#[test]
fn late_batch_failure_rolls_back_and_oversized_requests_are_not_retained() {
    let mut inventory = Inventory::default();
    inventory.give(BlockKind::Grass, 36).unwrap();
    let before = inventory.state.clone();
    let mut actions = Vec::new();
    for offset in 0..63 {
        let source_id = if offset % 2 == 0 {
            before.slots[0].unwrap().net_id
        } else {
            0
        };
        let target_id = if offset % 2 == 1 {
            before.slots[0].unwrap().net_id
        } else {
            0
        };
        actions.push(Action::Swap {
            source: reference(Slot::Inventory(0), source_id),
            destination: reference(Slot::Inventory(1), target_id),
        });
    }
    actions.push(Action::Unsupported);
    let request = Request { id: -1, actions };
    assert_eq!(
        inventory.apply(&request, true).result,
        Err(RejectReason::UnsupportedAction)
    );
    assert_eq!(inventory.state, before);
    assert!(inventory.apply(&request, true).replayed);
    let history = inventory.history.clone();
    let oversized = Request {
        id: -2,
        actions: vec![Action::Unsupported; 65],
    };
    assert_eq!(
        inventory.apply(&oversized, true).result,
        Err(RejectReason::TooManyActions)
    );
    assert_eq!(inventory.history, history);
    assert_eq!(inventory.state, before);
    assert_eq!(
        inventory.apply(&oversized, true).result,
        Err(RejectReason::StaleRequest)
    );
}

#[test]
fn history_bounds_action_storage_and_keeps_evicted_ids_stale() {
    let mut inventory = Inventory::default();
    let mine = Action::Mine {
        source: reference(Slot::Inventory(0), 0),
    };
    for sequence in 1..=33 {
        assert!(
            inventory
                .apply(
                    &Request {
                        id: -sequence,
                        actions: vec![mine.clone(); 64]
                    },
                    true
                )
                .result
                .is_ok()
        );
    }
    assert_eq!(inventory.history.len(), 32);
    assert_eq!(inventory.history_actions, HISTORY_ACTIONS);
    assert_eq!(
        inventory
            .apply(
                &Request {
                    id: -1,
                    actions: vec![mine.clone(); 64]
                },
                true
            )
            .result,
        Err(RejectReason::StaleRequest)
    );
    assert_eq!(
        inventory
            .apply(
                &Request {
                    id: -34,
                    actions: vec![Action::Mine {
                        source: reference(Slot::Inventory(0), -1)
                    }]
                },
                true
            )
            .result,
        Err(RejectReason::MissingPrediction)
    );
    for sequence in 35..=300 {
        let count = [0, 1, 32, 64][sequence as usize % 4];
        inventory.apply(
            &Request {
                id: -sequence,
                actions: vec![mine.clone(); count],
            },
            sequence % 2 == 0,
        );
        assert!(inventory.history.len() <= REQUEST_HISTORY);
        assert!(inventory.history_actions <= HISTORY_ACTIONS);
        assert_eq!(
            inventory.history_actions,
            inventory
                .history
                .iter()
                .map(|e| e.request.actions.len())
                .sum()
        );
    }
}
