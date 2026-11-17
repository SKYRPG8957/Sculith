use super::*;

fn set(inventory: &mut Inventory, slot: u8, kind: BlockKind, count: u8) {
    inventory
        .replace_creative(slot, Some(kind), count, true)
        .unwrap();
}

fn move_stack(inventory: &mut Inventory, slot: u8) -> Vec<Change> {
    inventory.click(Click::QuickMove { slot }, true).unwrap()
}

#[test]
fn quick_move_merges_before_empty_slots_in_both_directions() {
    for (source, matching, empty) in [(0, 12, 9), (9, 4, 0)] {
        let mut inventory = Inventory::default();
        set(&mut inventory, source, BlockKind::Dirt, 21);
        set(&mut inventory, matching, BlockKind::Dirt, 60);
        let old = inventory.slots()[matching as usize].unwrap().net_id;
        let changes = move_stack(&mut inventory, source);
        assert_eq!(
            changes.iter().map(|c| c.slot).collect::<Vec<_>>(),
            [
                Slot::Inventory(matching),
                Slot::Inventory(empty),
                Slot::Inventory(source)
            ]
        );
        assert_eq!(inventory.slots()[matching as usize].unwrap().count, 64);
        assert_eq!(inventory.slots()[matching as usize].unwrap().net_id, old);
        assert_eq!(inventory.slots()[empty as usize].unwrap().count, 17);
        assert!(inventory.slots()[source as usize].is_none());
        let occupied: Vec<_> = inventory.slots().iter().flatten().collect();
        assert_eq!(occupied.iter().map(|s| u16::from(s.count)).sum::<u16>(), 81);
        assert_ne!(occupied[0].net_id, occupied[1].net_id);
    }
}

#[test]
fn quick_move_partially_fills_and_a_full_destination_does_not_allocate_ids() {
    let mut inventory = Inventory::default();
    for slot in 0..9 {
        set(&mut inventory, slot, BlockKind::Stone, 64);
    }
    set(&mut inventory, 4, BlockKind::Dirt, 62);
    set(&mut inventory, 9, BlockKind::Dirt, 10);
    move_stack(&mut inventory, 9);
    assert_eq!(inventory.slots()[4].unwrap().count, 64);
    assert_eq!(inventory.slots()[9].unwrap().count, 8);
    let before = inventory.clone();
    assert!(move_stack(&mut inventory, 9).is_empty());
    assert_eq!(inventory, before);
    assert!(move_stack(&mut inventory, 35).is_empty());
    assert_eq!(inventory, before);
}

#[test]
fn quick_move_needs_no_new_ids_and_rejects_invalid_access() {
    let mut inventory = Inventory::default();
    set(&mut inventory, 0, BlockKind::Dirt, 10);
    set(&mut inventory, 12, BlockKind::Dirt, 63);
    let source_id = inventory.slots()[0].unwrap().net_id;
    inventory.state.next_stack_id = i32::MAX as u64 + 1;
    move_stack(&mut inventory, 0);
    assert_eq!(inventory.slots()[12].unwrap().count, 64);
    assert_eq!(inventory.slots()[9].unwrap().count, 9);
    assert_eq!(inventory.slots()[9].unwrap().net_id, source_id);
    assert_eq!(inventory.state.next_stack_id, i32::MAX as u64 + 1);
    let before = inventory.clone();
    for slot in [36, 255] {
        assert!(inventory.click(Click::QuickMove { slot }, true).is_err());
        assert_eq!(inventory, before);
    }
    assert!(
        inventory
            .click(Click::QuickMove { slot: 0 }, false)
            .is_err()
    );
    assert_eq!(inventory, before);
}

#[test]
fn quick_move_preserves_cursor_output_and_request_history() {
    let mut inventory = Inventory::default();
    set(&mut inventory, 0, BlockKind::Grass, 20);
    inventory
        .click(
            Click::Pickup {
                slot: 0,
                right: true,
            },
            true,
        )
        .unwrap();
    let request = Request {
        id: -1,
        actions: vec![Action::CraftCreative { catalogue_id: 1 }],
    };
    assert!(inventory.apply(&request, true).result.is_ok());
    let before = inventory.clone();
    move_stack(&mut inventory, 0);
    assert_eq!(inventory.cursor(), before.cursor());
    assert_eq!(inventory.created_output(), before.created_output());
    assert_eq!(inventory.history, before.history);
    assert_eq!(inventory.lowest_request_id, before.lowest_request_id);
    assert_eq!(inventory.slots()[9].unwrap().count, 10);
    let after = inventory.clone();
    assert!(inventory.apply(&request, true).replayed);
    assert_eq!(inventory, after);
}
