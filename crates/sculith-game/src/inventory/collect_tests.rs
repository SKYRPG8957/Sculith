use super::*;

fn set(inventory: &mut Inventory, slot: u8, count: u8) {
    inventory
        .replace_creative(slot, Some(BlockKind::Grass), count, true)
        .unwrap();
}

fn with_cursor(count: u8) -> Inventory {
    let mut inventory = Inventory::default();
    set(&mut inventory, 5, count);
    inventory
        .click(
            Click::Pickup {
                slot: 5,
                right: false,
            },
            true,
        )
        .unwrap();
    inventory
}

fn collect(inventory: &mut Inventory, reverse: bool) -> Vec<Change> {
    inventory
        .click(Click::Collect { slot: 5, reverse }, true)
        .unwrap()
}

#[test]
fn collect_partial_stacks_before_full_stacks_and_preserve_surviving_ids() {
    let mut inventory = with_cursor(5);
    set(&mut inventory, 9, 64);
    set(&mut inventory, 10, 20);
    set(&mut inventory, 0, 50);
    let old_ids: Vec<_> = inventory
        .slots()
        .iter()
        .flatten()
        .map(|s| s.net_id)
        .chain(inventory.cursor().map(|s| s.net_id))
        .collect();
    let changes = collect(&mut inventory, false);
    assert_eq!(
        changes.iter().map(|c| c.slot).collect::<Vec<_>>(),
        [Slot::Inventory(10), Slot::Inventory(0), Slot::Cursor]
    );
    assert_eq!(inventory.slots()[9].unwrap().count, 64);
    assert!(inventory.slots()[10].is_none());
    assert_eq!(inventory.slots()[0].unwrap().count, 11);
    assert_eq!(inventory.cursor().unwrap().count, 64);
    let all: Vec<_> = inventory
        .slots()
        .iter()
        .flatten()
        .copied()
        .chain(inventory.cursor())
        .collect();
    assert_eq!(all.iter().map(|s| u16::from(s.count)).sum::<u16>(), 139);
    for change in changes.iter().filter_map(|c| c.stack) {
        assert!(old_ids.contains(&change.net_id));
    }
    assert_ne!(
        inventory.slots()[0].unwrap().net_id,
        inventory.cursor().unwrap().net_id
    );
}

#[test]
fn collect_direction_reverses_main_and_hotbar_order_without_exceeding_capacity() {
    for reverse in [false, true] {
        let mut inventory = with_cursor(60);
        set(&mut inventory, 9, 3);
        set(&mut inventory, 8, 3);
        collect(&mut inventory, reverse);
        assert_eq!(inventory.cursor().unwrap().count, 64);
        let (empty, remaining) = if reverse { (8, 9) } else { (9, 8) };
        assert!(inventory.slots()[empty].is_none());
        assert_eq!(inventory.slots()[remaining].unwrap().count, 2);
    }
}

#[test]
fn collect_covers_every_inventory_slot_once_without_allocating_ids() {
    let mut inventory = with_cursor(1);
    for slot in 0..INVENTORY_SLOTS as u8 {
        if slot != 5 {
            set(&mut inventory, slot, 1);
        }
    }
    let mut exhausted = inventory.clone();
    exhausted.state.next_stack_id = i32::MAX as u64 + 1;
    let changes = collect(&mut exhausted, false);
    assert_eq!(exhausted.state.next_stack_id, i32::MAX as u64 + 1);
    assert_eq!(changes.len(), INVENTORY_SLOTS);
    assert_eq!(
        exhausted.cursor().unwrap().net_id,
        inventory.cursor().unwrap().net_id
    );
    let changes = collect(&mut inventory, false);
    assert_eq!(changes.len(), INVENTORY_SLOTS);
    assert_eq!(
        changes
            .iter()
            .map(|c| c.slot)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        INVENTORY_SLOTS
    );
    assert!(!changes.iter().any(|c| c.slot == Slot::Inventory(5)));
    assert!(inventory.slots().iter().all(Option::is_none));
    assert_eq!(inventory.cursor().unwrap().count as usize, INVENTORY_SLOTS);
}

#[test]
fn collect_empty_full_occupied_or_incompatible_cursor_cases_do_not_mutate_or_allocate() {
    let mut cases = Vec::new();
    let mut empty = Inventory::default();
    set(&mut empty, 9, 20);
    cases.push(empty);
    let mut full = with_cursor(64);
    set(&mut full, 9, 20);
    cases.push(full);
    let mut occupied = with_cursor(1);
    set(&mut occupied, 5, 2);
    cases.push(occupied);
    let mut different = with_cursor(1);
    different
        .replace_creative(9, Some(BlockKind::Dirt), 20, true)
        .unwrap();
    cases.push(different);
    let mut projected = with_cursor(1);
    assert!(
        projected
            .apply(
                &Request {
                    id: -1,
                    actions: vec![Action::CraftCreative { catalogue_id: 3 }]
                },
                true
            )
            .result
            .is_ok()
    );
    cases.push(projected); // Matching created output is not an owned inventory source.
    for mut inventory in cases {
        let before = inventory.clone();
        assert!(collect(&mut inventory, false).is_empty());
        assert_eq!(inventory, before);
    }
}

#[test]
fn collect_rejects_invalid_access_and_preserves_ids_output_and_history() {
    let mut inventory = with_cursor(62);
    set(&mut inventory, 9, 3);
    assert!(
        inventory
            .apply(
                &Request {
                    id: -1,
                    actions: vec![Action::CraftCreative { catalogue_id: 1 }]
                },
                true
            )
            .result
            .is_ok()
    );
    let previous = inventory.clone();
    inventory.state.next_stack_id = i32::MAX as u64 + 1;
    collect(&mut inventory, false);
    assert_eq!(inventory.state.next_stack_id, i32::MAX as u64 + 1);
    assert_eq!(
        inventory.cursor().unwrap().net_id,
        previous.cursor().unwrap().net_id
    );
    assert_eq!(
        inventory.slots()[9].unwrap().net_id,
        previous.slots()[9].unwrap().net_id
    );
    let exhausted = inventory.clone();
    for slot in [36, 255] {
        assert!(
            inventory
                .click(
                    Click::Collect {
                        slot,
                        reverse: false
                    },
                    true
                )
                .is_err()
        );
        assert_eq!(inventory, exhausted);
    }
    assert!(
        inventory
            .click(
                Click::Collect {
                    slot: 5,
                    reverse: false
                },
                false
            )
            .is_err()
    );
    assert_eq!(inventory, exhausted);
    inventory = previous.clone();
    collect(&mut inventory, false);
    assert_eq!(inventory.created_output(), previous.created_output());
    assert_eq!(inventory.history, previous.history);
    assert_eq!(inventory.lowest_request_id, previous.lowest_request_id);
    assert_eq!(inventory.slots()[9].unwrap().count, 1);
}
