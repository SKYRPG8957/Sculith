use super::*;

#[test]
#[ignore = "manual inventory work/history measurement"]
fn inventory_batch_measurement() {
    use std::{hint::black_box, mem::size_of, time::Instant};
    for actions in [32, 64].into_iter().filter(|&n| n <= MAX_ACTIONS) {
        let mut inventory = fixture_inventory();
        let start = Instant::now();
        for sequence in 1..=20_000 {
            let id = -sequence;
            let mut batch = vec![
                Action::Swap {
                    source: reference(Slot::Inventory(0), id),
                    destination: reference(Slot::Inventory(1), id),
                };
                actions
            ];
            batch[0] = Action::Swap {
                source: exact(&inventory, Slot::Inventory(0)),
                destination: exact(&inventory, Slot::Inventory(1)),
            };
            assert!(
                black_box(inventory.apply(&Request { id, actions: batch }, true))
                    .result
                    .is_ok()
            );
        }
        let elapsed = start.elapsed();
        let cached_actions: usize = inventory
            .history
            .iter()
            .map(|e| e.request.actions.len())
            .sum();
        let action_bytes: usize = inventory
            .history
            .iter()
            .map(|e| e.request.actions.capacity() * size_of::<Action>())
            .sum();
        let response_bytes: usize = inventory
            .history
            .iter()
            .map(|e| {
                e.response
                    .result
                    .as_ref()
                    .map_or(0, |c| c.capacity() * size_of::<Change>())
            })
            .sum();
        println!(
            "inventory_batch actions={actions} requests=20000 wall_ns={} inventory_bytes={} entries={} cached_actions={cached_actions} action_bytes={action_bytes} response_bytes={response_bytes}",
            elapsed.as_nanos(),
            size_of::<Inventory>(),
            inventory.history.len()
        );
    }
}

fn fixture_inventory() -> Inventory {
    let mut inventory = Inventory::default();
    for kind in CREATIVE_CATALOGUE[..3].iter().copied() {
        inventory.give(kind, 64).unwrap();
    }
    inventory
}

#[test]
fn new_inventory_is_empty_and_first_grant_allocates_fresh_stack_identity() {
    let mut inventory = Inventory::default();
    assert!(inventory.slots().iter().all(Option::is_none));
    assert!(inventory.cursor().is_none() && inventory.created_output().is_none());
    assert!(inventory.history.is_empty() && inventory.lowest_request_id.is_none());
    assert_eq!(inventory.history.capacity(), 0);
    inventory.give(BlockKind::Stone, 5).unwrap();
    let first = inventory.slots()[0].unwrap();
    assert_eq!(
        (first.kind, first.count, first.net_id),
        (BlockKind::Stone, 5, 1)
    );
    assert!(inventory.slots()[1..].iter().all(Option::is_none));
    assert_eq!(inventory.history.capacity(), 0);
    let rejected = inventory.apply(
        &Request {
            id: -1,
            actions: vec![],
        },
        true,
    );
    assert_eq!(rejected.result, Err(RejectReason::EmptyRequest));
    assert_eq!(inventory.history.len(), 1);
}

#[test]
fn server_grants_merge_before_empty_slots_and_preserve_cursor_output_and_history() {
    let mut inventory = fixture_inventory();
    inventory.replace_creative(0, None, 0, true).unwrap();
    inventory
        .replace_creative(8, Some(BlockKind::Dirt), 60, true)
        .unwrap();
    inventory
        .click(
            Click::Pickup {
                slot: 2,
                right: true,
            },
            true,
        )
        .unwrap();
    let cursor = inventory.cursor();
    let history = inventory.history.clone();
    let changes = inventory.give(BlockKind::Dirt, 10).unwrap();
    assert_eq!(
        changes.iter().map(|c| c.slot).collect::<Vec<_>>(),
        [Slot::Inventory(8), Slot::Inventory(0)]
    );
    assert_eq!(inventory.slots()[8].unwrap().count, 64);
    assert_eq!(inventory.slots()[0].unwrap().count, 6);
    assert_eq!(inventory.cursor(), cursor);
    assert_eq!(inventory.history, history);
    assert!(changes.iter().all(|c| c.stack.unwrap().net_id > 0));
}

#[test]
fn server_grants_roll_back_full_inventory_and_allocator_exhaustion() {
    let mut inventory = fixture_inventory();
    for slot in 0..36 {
        inventory
            .replace_creative(slot, Some(BlockKind::Stone), 64, true)
            .unwrap();
    }
    inventory
        .replace_creative(20, Some(BlockKind::Dirt), 63, true)
        .unwrap();
    let before = inventory.clone();
    assert_eq!(inventory.give(BlockKind::Dirt, 2), Err(RejectReason::Full));
    assert_eq!(inventory, before);
    assert_eq!(
        inventory.give(BlockKind::Air, 1),
        Err(RejectReason::UnsupportedItem)
    );
    assert_eq!(
        inventory.give(BlockKind::Dirt, 0),
        Err(RejectReason::InvalidCount)
    );
    assert_eq!(inventory, before);
    let mut inventory = fixture_inventory();
    inventory.state.next_stack_id = i32::MAX as u64;
    let before = inventory.clone();
    assert_eq!(
        inventory.give(BlockKind::Bedrock, 65),
        Err(RejectReason::StackIdExhausted)
    );
    assert_eq!(inventory, before);
}

#[test]
fn server_grants_cannot_be_undone_by_replayed_client_requests() {
    let mut inventory = fixture_inventory();
    let request = Request {
        id: -1,
        actions: vec![Action::Transfer {
            source: reference(Slot::Inventory(0), 1),
            destination: reference(Slot::Inventory(3), 0),
            count: 10,
        }],
    };
    let original = inventory.apply(&request, true);
    assert!(original.result.is_ok());
    inventory.give(BlockKind::Stone, 5).unwrap();
    let after = inventory.clone();
    assert!(inventory.apply(&request, true).replayed);
    assert_eq!(inventory, after);
    let stale = Request {
        id: -2,
        actions: vec![Action::Destroy {
            source: reference(Slot::Inventory(0), -1),
            count: 1,
        }],
    };
    assert_eq!(
        inventory.apply(&stale, true).result,
        Err(RejectReason::StackMismatch)
    );
    assert_eq!(inventory.state, after.state);
}

fn reference(slot: Slot, expected_id: i32) -> SlotRef {
    SlotRef { slot, expected_id }
}

fn exact(inventory: &Inventory, slot: Slot) -> SlotRef {
    reference(
        slot,
        inventory.get(slot).unwrap().map_or(0, |stack| stack.net_id),
    )
}

fn apply(inventory: &mut Inventory, id: i32, actions: Vec<Action>) -> Vec<Change> {
    inventory
        .apply(&Request { id, actions }, true)
        .result
        .unwrap()
}

fn transfer(source: SlotRef, destination: SlotRef, count: u8) -> Action {
    Action::Transfer {
        source,
        destination,
        count,
    }
}

#[test]
fn split_merge_swap_and_destroy_preserve_counts_and_unique_ids() {
    let mut inventory = fixture_inventory();
    assert_eq!(inventory.slots().len(), 36);
    let source = exact(&inventory, Slot::Inventory(0));
    apply(
        &mut inventory,
        -1,
        vec![transfer(source, reference(Slot::Cursor, 0), 32)],
    );
    assert_eq!(inventory.selected(0).unwrap().unwrap().count, 32);
    assert_eq!(inventory.cursor().unwrap().count, 32);
    let source = exact(&inventory, Slot::Cursor);
    let destination = exact(&inventory, Slot::Inventory(0));
    apply(&mut inventory, -2, vec![transfer(source, destination, 7)]);
    assert_eq!(inventory.selected(0).unwrap().unwrap().count, 39);
    assert_eq!(inventory.cursor().unwrap().count, 25);
    let source = exact(&inventory, Slot::Cursor);
    let destination = exact(&inventory, Slot::Inventory(1));
    apply(
        &mut inventory,
        -3,
        vec![Action::Swap {
            source,
            destination,
        }],
    );
    assert_eq!(inventory.cursor().unwrap().kind, BlockKind::Dirt);
    assert_eq!(
        inventory.get(Slot::Inventory(1)).unwrap().unwrap().count,
        25
    );
    let source = exact(&inventory, Slot::Cursor);
    apply(
        &mut inventory,
        -4,
        vec![Action::Destroy { source, count: 64 }],
    );
    assert_eq!(inventory.cursor(), None);
    let stacks: Vec<_> = inventory.slots().iter().flatten().collect();
    assert_eq!(
        stacks
            .iter()
            .map(|stack| u32::from(stack.count))
            .sum::<u32>(),
        128
    );
    let mut ids: Vec<_> = stacks.iter().map(|stack| stack.net_id).collect();
    assert!(ids.iter().all(|id| *id > 0));
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), stacks.len());
}

#[test]
fn atomic_failure_restores_stacks_and_allocator() {
    let mut inventory = fixture_inventory();
    let before = inventory.state.clone();
    let request = Request {
        id: -1,
        actions: vec![
            Action::CraftCreative { catalogue_id: 4 },
            transfer(
                reference(Slot::CreatedOutput, -1),
                reference(Slot::Inventory(9), 0),
                64,
            ),
            Action::Destroy {
                source: reference(Slot::Inventory(9), -1),
                count: 65,
            },
        ],
    };
    assert_eq!(
        inventory.apply(&request, true).result,
        Err(RejectReason::InvalidCount)
    );
    assert_eq!(inventory.state, before);
    assert!(inventory.apply(&request, true).replayed);
    assert_eq!(inventory.state, before);
}

#[test]
fn unsupported_adapter_action_is_an_atomic_cached_rejection() {
    let mut inventory = fixture_inventory();
    let before = inventory.state.clone();
    let request = Request {
        id: -1,
        actions: vec![
            Action::CraftCreative { catalogue_id: 1 },
            Action::Unsupported,
        ],
    };
    assert_eq!(
        inventory.apply(&request, true).result,
        Err(RejectReason::UnsupportedAction)
    );
    assert_eq!(inventory.state, before);
    assert!(inventory.apply(&request, true).replayed);
    assert_eq!(inventory.state, before);
}

#[test]
fn predictions_are_scoped_to_changed_slots_and_current_or_retained_request() {
    let mut inventory = fixture_inventory();
    apply(
        &mut inventory,
        -1,
        vec![
            Action::CraftCreative { catalogue_id: 4 },
            transfer(
                reference(Slot::CreatedOutput, -1),
                reference(Slot::Cursor, 0),
                64,
            ),
            transfer(
                reference(Slot::Cursor, -1),
                reference(Slot::Inventory(9), 0),
                1,
            ),
        ],
    );
    assert_eq!(inventory.cursor().unwrap().count, 63);
    assert_eq!(
        inventory.get(Slot::Inventory(9)).unwrap().unwrap().kind,
        BlockKind::Bedrock
    );
    apply(
        &mut inventory,
        -2,
        vec![transfer(
            reference(Slot::Cursor, -1),
            reference(Slot::Inventory(9), -1),
            2,
        )],
    );
    assert_eq!(inventory.get(Slot::Inventory(9)).unwrap().unwrap().count, 3);
    let before = inventory.state.clone();
    let result = inventory.apply(
        &Request {
            id: -3,
            actions: vec![Action::Mine {
                source: reference(Slot::Inventory(0), -1),
            }],
        },
        true,
    );
    assert_eq!(result.result, Err(RejectReason::MissingPrediction));
    assert_eq!(inventory.state, before);
    let result = inventory.apply(
        &Request {
            id: -4,
            actions: vec![transfer(
                reference(Slot::Cursor, -1),
                reference(Slot::Inventory(10), 0),
                1,
            )],
        },
        true,
    );
    assert_eq!(result.result, Err(RejectReason::StackMismatch));
    assert_eq!(inventory.state, before);
}

#[test]
fn stale_positive_empty_and_future_claims_reject() {
    let mut inventory = fixture_inventory();
    for (id, expected_id, expected_error) in [
        (-1, 12345, RejectReason::StackMismatch),
        (-2, 0, RejectReason::StackMismatch),
        (-3, -4, RejectReason::MissingPrediction),
    ] {
        let before = inventory.state.clone();
        let response = inventory.apply(
            &Request {
                id,
                actions: vec![Action::Mine {
                    source: reference(Slot::Inventory(0), expected_id),
                }],
            },
            true,
        );
        assert_eq!(response.result, Err(expected_error));
        assert_eq!(inventory.state, before);
    }
}

#[test]
fn request_replay_is_equal_bounded_and_never_fresh_after_eviction() {
    let mut inventory = fixture_inventory();
    let first = Request {
        id: -1,
        actions: vec![Action::CraftCreative { catalogue_id: 1 }],
    };
    let response = inventory.apply(&first, true);
    let state = inventory.state.clone();
    let replay = inventory.apply(&first, true);
    assert!(replay.replayed);
    assert_eq!(replay.result, response.result);
    assert_eq!(inventory.state, state);
    let conflict = Request {
        id: -1,
        actions: vec![Action::CraftCreative { catalogue_id: 2 }],
    };
    assert_eq!(
        inventory.apply(&conflict, true).result,
        Err(RejectReason::RequestIdConflict)
    );
    for sequence in 2..=(REQUEST_HISTORY as i32 + 1) {
        let id = -sequence;
        let source = exact(&inventory, Slot::Inventory(0));
        apply(&mut inventory, id, vec![Action::Mine { source }]);
    }
    assert_eq!(inventory.history.len(), REQUEST_HISTORY);
    let state = inventory.state.clone();
    assert_eq!(
        inventory.apply(&first, true).result,
        Err(RejectReason::StaleRequest)
    );
    assert_eq!(inventory.state, state);
}

#[test]
fn rejects_invalid_catalogue_count_slot_and_permissions() {
    let cases = [
        (
            Action::CraftCreative { catalogue_id: 0 },
            RejectReason::InvalidCatalogue,
        ),
        (
            Action::CraftCreative { catalogue_id: 5 },
            RejectReason::InvalidCatalogue,
        ),
        (
            Action::Destroy {
                source: reference(Slot::Inventory(0), 1),
                count: 0,
            },
            RejectReason::InvalidCount,
        ),
        (
            transfer(
                reference(Slot::Inventory(36), 0),
                reference(Slot::Cursor, 0),
                1,
            ),
            RejectReason::InvalidSlot,
        ),
        (
            transfer(
                reference(Slot::Inventory(0), 1),
                reference(Slot::Inventory(0), 1),
                1,
            ),
            RejectReason::SameSlot,
        ),
        (
            transfer(
                reference(Slot::Inventory(0), 1),
                reference(Slot::Inventory(1), 2),
                1,
            ),
            RejectReason::IncompatibleStack,
        ),
        (
            transfer(
                reference(Slot::Inventory(0), 1),
                reference(Slot::CreatedOutput, 0),
                1,
            ),
            RejectReason::InvalidDestination,
        ),
        (
            Action::Mine {
                source: reference(Slot::Inventory(9), 0),
            },
            RejectReason::InvalidMiningSlot,
        ),
    ];
    for (action, error) in cases {
        let mut inventory = fixture_inventory();
        let before = inventory.state.clone();
        assert_eq!(
            inventory
                .apply(
                    &Request {
                        id: -1,
                        actions: vec![action]
                    },
                    true
                )
                .result,
            Err(error)
        );
        assert_eq!(inventory.state, before);
    }
    let mut inventory = fixture_inventory();
    let before = inventory.state.clone();
    assert_eq!(
        inventory
            .apply(
                &Request {
                    id: -1,
                    actions: vec![Action::CraftCreative { catalogue_id: 1 }]
                },
                false
            )
            .result,
        Err(RejectReason::Unauthorized)
    );
    assert_eq!(inventory.state, before);
}

#[test]
fn merge_overflow_and_excessive_removal_are_atomic() {
    let mut inventory = fixture_inventory();
    inventory
        .replace_creative(9, Some(BlockKind::Stone), 1, true)
        .unwrap();
    let before = inventory.state.clone();
    let source = exact(&inventory, Slot::Inventory(9));
    let destination = exact(&inventory, Slot::Inventory(0));
    assert_eq!(
        inventory
            .apply(
                &Request {
                    id: -1,
                    actions: vec![transfer(source, destination, 1)]
                },
                true
            )
            .result,
        Err(RejectReason::StackOverflow)
    );
    assert_eq!(inventory.state, before);
    assert_eq!(
        inventory
            .apply(
                &Request {
                    id: -2,
                    actions: vec![Action::Destroy { source, count: 2 }]
                },
                true
            )
            .result,
        Err(RejectReason::InvalidCount)
    );
    assert_eq!(inventory.state, before);
}

#[test]
fn creative_replacement_and_removal_enforce_the_same_authority_and_allowlist() {
    let mut inventory = fixture_inventory();
    inventory
        .replace_creative(35, Some(BlockKind::Bedrock), 17, true)
        .unwrap();
    assert_eq!(
        inventory.get(Slot::Inventory(35)).unwrap().unwrap().count,
        17
    );
    let before = inventory.state.clone();
    for (slot, kind, count, authorized) in [
        (0, Some(BlockKind::Stone), 1, false),
        (36, Some(BlockKind::Stone), 1, true),
        (0, Some(BlockKind::Air), 1, true),
        (0, Some(BlockKind::Stone), 0, true),
        (0, Some(BlockKind::Stone), 65, true),
        (0, None, 1, true),
    ] {
        assert!(
            inventory
                .replace_creative(slot, kind, count, authorized)
                .is_err()
        );
        assert_eq!(inventory.state, before);
    }
    inventory.replace_creative(35, None, 0, true).unwrap();
    assert_eq!(inventory.get(Slot::Inventory(35)).unwrap(), None);
    assert!(
        inventory
            .replace_creative(35, None, 0, true)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn last_positive_id_is_usable_and_exhaustion_rolls_back_partial_allocation() {
    let mut inventory = fixture_inventory();
    inventory.state.next_stack_id = i32::MAX as u64;
    let source = exact(&inventory, Slot::Inventory(0));
    let before = inventory.state.clone();
    let request = Request {
        id: -1,
        actions: vec![transfer(source, reference(Slot::Cursor, 0), 1)],
    };
    assert_eq!(
        inventory.apply(&request, true).result,
        Err(RejectReason::StackIdExhausted)
    );
    assert_eq!(inventory.state, before);
    apply(
        &mut inventory,
        -2,
        vec![Action::CraftCreative { catalogue_id: 4 }],
    );
    assert_eq!(inventory.created_output().unwrap().net_id, i32::MAX);
    let before = inventory.state.clone();
    assert!(
        inventory
            .replace_creative(9, Some(BlockKind::Stone), 1, true)
            .is_err()
    );
    assert_eq!(inventory.state, before);
}

#[test]
fn action_and_sequence_limits_do_not_wrap_or_clone_oversized_requests() {
    let mut inventory = fixture_inventory();
    let before = inventory.state.clone();
    let action = Action::CraftCreative { catalogue_id: 1 };
    let request = Request {
        id: -1,
        actions: vec![action; MAX_ACTIONS + 1],
    };
    assert_eq!(
        inventory.apply(&request, true).result,
        Err(RejectReason::TooManyActions)
    );
    assert!(inventory.history.is_empty());
    assert_eq!(inventory.state, before);
    for id in [0, 1] {
        assert_eq!(
            inventory
                .apply(
                    &Request {
                        id,
                        actions: vec![]
                    },
                    true
                )
                .result,
            Err(RejectReason::InvalidRequestId)
        );
    }
    let source = exact(&inventory, Slot::Inventory(0));
    apply(&mut inventory, i32::MIN, vec![Action::Mine { source }]);
    assert_eq!(
        inventory
            .apply(
                &Request {
                    id: -2,
                    actions: vec![]
                },
                true
            )
            .result,
        Err(RejectReason::StaleRequest)
    );
}

#[test]
fn pickup_clicks_split_place_merge_and_swap_from_owner_counts() {
    let mut inventory = fixture_inventory();
    inventory
        .replace_creative(9, Some(BlockKind::Stone), 5, true)
        .unwrap();
    inventory
        .click(
            Click::Pickup {
                slot: 9,
                right: true,
            },
            true,
        )
        .unwrap();
    assert_eq!(inventory.cursor().unwrap().count, 3);
    assert_eq!(inventory.get(Slot::Inventory(9)).unwrap().unwrap().count, 2);
    inventory
        .click(
            Click::Pickup {
                slot: 10,
                right: true,
            },
            true,
        )
        .unwrap();
    assert_eq!(inventory.cursor().unwrap().count, 2);
    assert_eq!(
        inventory.get(Slot::Inventory(10)).unwrap().unwrap().count,
        1
    );
    inventory
        .click(
            Click::Pickup {
                slot: 9,
                right: false,
            },
            true,
        )
        .unwrap();
    assert_eq!(inventory.cursor(), None);
    assert_eq!(inventory.get(Slot::Inventory(9)).unwrap().unwrap().count, 4);
    inventory
        .click(
            Click::Pickup {
                slot: 9,
                right: false,
            },
            true,
        )
        .unwrap();
    assert_eq!(inventory.get(Slot::Inventory(9)).unwrap(), None);
    assert_eq!(inventory.cursor().unwrap().count, 4);
    inventory
        .click(
            Click::Pickup {
                slot: 1,
                right: true,
            },
            true,
        )
        .unwrap();
    assert_eq!(inventory.cursor().unwrap().kind, BlockKind::Dirt);
    assert_eq!(inventory.get(Slot::Inventory(1)).unwrap().unwrap().count, 4);
    inventory
        .click(
            Click::Pickup {
                slot: 11,
                right: false,
            },
            true,
        )
        .unwrap();
    assert_eq!(inventory.cursor(), None);
    assert_eq!(
        inventory.get(Slot::Inventory(11)).unwrap().unwrap().count,
        64
    );
    // No click consumes or creates items, including incompatible right-click swaps.
    assert_eq!(
        inventory
            .slots()
            .iter()
            .flatten()
            .map(|stack| u32::from(stack.count))
            .sum::<u32>(),
        197
    );
    assert_eq!(inventory.lowest_request_id, None);
    assert!(inventory.history.is_empty());
}

#[test]
fn pickup_full_empty_and_capacity_limits_have_exact_noop_behavior() {
    let mut inventory = fixture_inventory();
    let before = inventory.clone();
    assert!(
        inventory
            .click(
                Click::Pickup {
                    slot: 9,
                    right: false
                },
                true
            )
            .unwrap()
            .is_empty()
    );
    assert_eq!(inventory, before);
    inventory
        .click(
            Click::Pickup {
                slot: 0,
                right: true,
            },
            true,
        )
        .unwrap();
    inventory
        .replace_creative(9, Some(BlockKind::Stone), 63, true)
        .unwrap();
    inventory
        .click(
            Click::Pickup {
                slot: 9,
                right: false,
            },
            true,
        )
        .unwrap();
    assert_eq!(
        inventory.get(Slot::Inventory(9)).unwrap().unwrap().count,
        64
    );
    assert_eq!(inventory.cursor().unwrap().count, 31);
    let before = inventory.clone();
    for right in [false, true] {
        assert!(
            inventory
                .click(Click::Pickup { slot: 9, right }, true)
                .unwrap()
                .is_empty()
        );
        assert_eq!(inventory, before);
    }
    inventory
        .click(
            Click::Pickup {
                slot: 10,
                right: false,
            },
            true,
        )
        .unwrap();
    assert_eq!(inventory.cursor(), None);
    assert_eq!(
        inventory.get(Slot::Inventory(10)).unwrap().unwrap().count,
        31
    );
}

#[test]
fn hotbar_swap_self_empty_and_bounds_preserve_authority() {
    let mut inventory = fixture_inventory();
    let before = inventory.clone();
    assert!(
        inventory
            .click(Click::HotbarSwap { slot: 0, hotbar: 0 }, true)
            .unwrap()
            .is_empty()
    );
    assert_eq!(inventory, before);
    assert!(
        inventory
            .click(Click::HotbarSwap { slot: 9, hotbar: 8 }, true)
            .unwrap()
            .is_empty()
    );
    assert_eq!(inventory, before);
    inventory
        .click(Click::HotbarSwap { slot: 9, hotbar: 1 }, true)
        .unwrap();
    assert_eq!(inventory.get(Slot::Inventory(1)).unwrap(), None);
    assert_eq!(
        inventory.get(Slot::Inventory(9)).unwrap().unwrap().kind,
        BlockKind::Dirt
    );
    assert_eq!(inventory.cursor(), None);
    for click in [
        Click::Pickup {
            slot: 36,
            right: false,
        },
        Click::HotbarSwap {
            slot: 36,
            hotbar: 0,
        },
        Click::HotbarSwap { slot: 9, hotbar: 9 },
    ] {
        let before = inventory.clone();
        assert!(inventory.click(click, true).is_err());
        assert_eq!(inventory, before);
    }
    let before = inventory.clone();
    assert!(
        inventory
            .click(
                Click::Pickup {
                    slot: 9,
                    right: false
                },
                false
            )
            .is_err()
    );
    assert_eq!(inventory, before);
}

#[test]
fn click_stack_id_exhaustion_rolls_back_and_fresh_ids_are_unique() {
    let mut inventory = fixture_inventory();
    inventory.state.next_stack_id = i32::MAX as u64;
    let before = inventory.clone();
    assert!(
        inventory
            .click(
                Click::Pickup {
                    slot: 0,
                    right: true
                },
                true
            )
            .is_err()
    );
    assert_eq!(inventory, before);
    inventory.state.next_stack_id = 4;
    let old_ids: Vec<_> = inventory
        .slots()
        .iter()
        .flatten()
        .map(|stack| stack.net_id)
        .collect();
    let changes = inventory
        .click(
            Click::Pickup {
                slot: 0,
                right: true,
            },
            true,
        )
        .unwrap();
    assert!(
        changes
            .iter()
            .filter_map(|change| change.stack)
            .all(|stack| stack.net_id > 0 && !old_ids.contains(&stack.net_id))
    );
    let slot_id = inventory.selected(0).unwrap().unwrap().net_id;
    assert_ne!(inventory.cursor().unwrap().net_id, slot_id);
}
