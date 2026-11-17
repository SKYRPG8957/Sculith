//! Stock Java 26.3/protocol 777 creative-slot requests for the builder registry.
//!
//! Layout checked against the official 26.3 ServerboundSetCreativeModeSlotPacket,
//! ItemStack codecs and InventoryMenu, server jar SHA-1
//! 33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c. Item IDs are also recorded in the
//! pinned protocol777.json. The world owner, not this decoder, grants creative
//! permission and commits replacements. No component payload is executed here.

use std::collections::BTreeSet;

use anyhow::{Result, ensure};
use sculith_game::inventory::{Click, DragMode, ItemStack, MAX_STACK};
use sculith_world::BlockKind;

use super::codec::Fields;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct CreativeChange {
    pub slot: u8,
    pub kind: Option<BlockKind>,
    pub count: u8,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ClickChange {
    pub state_id: u16,
    pub click: Click,
}

fn owner_slot(menu_slot: i16) -> Option<u8> {
    match menu_slot {
        36..=44 => Some((menu_slot - 36) as u8),
        9..=35 => Some(menu_slot as u8),
        _ => None,
    }
}

fn item_kind(id: i32) -> Option<BlockKind> {
    match id {
        1 => Some(BlockKind::Stone),
        55 => Some(BlockKind::Dirt),
        54 => Some(BlockKind::Grass),
        87 => Some(BlockKind::Bedrock),
        _ => None,
    }
}

/// `None` is an unsupported bounded claim; the caller restores inventory and
/// keeps the connection usable. Framing already bounds the supplied fields.
/// Errors indicate malformed/truncated known fields or trailing supported data.
pub fn creative(mut fields: Fields<'_>) -> Result<Option<CreativeChange>> {
    let slot = owner_slot(fields.short()?);
    let count = fields.int()?;
    if count < 0 {
        return Ok(None);
    }
    if count == 0 {
        fields.end()?;
        return Ok(slot.map(|slot| CreativeChange {
            slot,
            kind: None,
            count: 0,
        }));
    }

    let item_id = fields.int()?;
    // The full ItemStack patch begins with both counts before either payload.
    let added_components = fields.int()?;
    let removed_components = fields.int()?;
    if added_components != 0 || removed_components != 0 {
        // Unknown component values have different codecs. Reject this bounded
        // frame without attempting to guess, skip, or apply their payloads.
        return Ok(None);
    }

    let kind = item_kind(item_id);
    let (Some(slot), Some(kind)) = (slot, kind) else {
        return Ok(None);
    };
    if !(1..=i32::from(MAX_STACK)).contains(&count) {
        return Ok(None);
    }
    fields.end()?;
    Ok(Some(CreativeChange {
        slot,
        kind: Some(kind),
        count: count as u8,
    }))
}

/// Decode protocol 777 click metadata and bounded HashedStack predictions.
/// Hashes are parsed solely for validation and are never replacement authority.
/// Ordinary stale clicks still use owner state. Stale drag phases cancel the
/// gesture; accepted intermediate phases retain the client preview.
pub fn click(mut fields: Fields<'_>) -> Result<Option<ClickChange>> {
    let container = fields.int()?;
    let state_id = fields.int()?;
    let menu_slot = fields.short()?;
    let slot = owner_slot(menu_slot);
    let button = fields.byte()?;
    let mode = fields.int()?;
    let mut supported = container == 0 && (0..=32767).contains(&state_id);
    let changed_count = collection_len(&mut fields, 128)?;
    let mut changed_slots = BTreeSet::new();
    for _ in 0..changed_count {
        let menu_slot = fields.short()?;
        ensure!(
            changed_slots.insert(menu_slot),
            "duplicate Java changed slot"
        );
        supported &= owner_slot(menu_slot).is_some();
        supported &= hashed_stack(&mut fields)?;
    }
    supported &= hashed_stack(&mut fields)?;
    fields.end()?;
    if !supported {
        return Ok(None);
    }
    if mode == 5 {
        let drag = match button >> 2 {
            0 => DragMode::Even,
            1 => DragMode::OneEach,
            _ => return Ok(None),
        };
        let click = match button & 3 {
            0 if menu_slot == -999 => Click::DragStart { mode: drag },
            1 => {
                let Some(slot) = slot else { return Ok(None) };
                Click::DragAdd { slot, mode: drag }
            }
            2 if menu_slot == -999 => Click::DragFinish { mode: drag },
            _ => return Ok(None),
        };
        return Ok(Some(ClickChange {
            state_id: state_id as u16,
            click,
        }));
    }
    let Some(slot) = slot else { return Ok(None) };
    let click = match (mode, button) {
        (6, 0..=1) => Click::Collect {
            slot,
            reverse: button == 1,
        },
        (1, 0..=1) => Click::QuickMove { slot },
        (0, 0..=1) => Click::Pickup {
            slot,
            right: button == 1,
        },
        (2, 0..=8) => Click::HotbarSwap {
            slot,
            hotbar: button,
        },
        _ => return Ok(None),
    };
    Ok(Some(ClickChange {
        state_id: state_id as u16,
        click,
    }))
}

fn collection_len(fields: &mut Fields<'_>, maximum: i32) -> Result<usize> {
    let count = fields.int()?;
    ensure!(
        (0..=maximum).contains(&count),
        "Java prediction collection exceeds bounds"
    );
    Ok(count as usize)
}

fn hashed_stack(fields: &mut Fields<'_>) -> Result<bool> {
    if !fields.boolean()? {
        return Ok(true);
    }
    // holderRegistry uses the direct item registry ID, without an offset.
    let item = fields.int()?;
    let count = fields.int()?;
    let mut supported = item_kind(item).is_some() && (1..=i32::from(MAX_STACK)).contains(&count);
    let mut component_ids = BTreeSet::new();
    // HashedPatchMap encodes its added map (including hashes) before the removed
    // set. Unlike full ItemStack patches, its two counts are not adjacent.
    for _ in 0..collection_len(fields, 256)? {
        let component = fields.int()?;
        ensure!(
            component_ids.insert(component),
            "duplicate Java component prediction"
        );
        supported &= component >= 0;
        fields.take(4)?; // Big-endian i32 hash, not a component value payload.
    }
    for _ in 0..collection_len(fields, 256)? {
        let component = fields.int()?;
        ensure!(
            component_ids.insert(component),
            "duplicate Java component prediction"
        );
        supported &= component >= 0;
    }
    Ok(supported)
}

/// Encode an authoritative clientbound optional full stack with empty patches.
/// Validation precedes output, so errors leave the caller's buffer untouched.
pub fn write_stack(stack: Option<ItemStack>, output: &mut Vec<u8>) -> Result<()> {
    let Some(stack) = stack else {
        super::write_varint(0, output);
        return Ok(());
    };
    ensure!(
        (1..=MAX_STACK).contains(&stack.count),
        "invalid owner stack count"
    );
    ensure!(stack.net_id > 0, "invalid owner stack ID");
    ensure!(
        stack.kind != BlockKind::Air,
        "air must be an empty inventory slot"
    );
    let item_id = super::registry::get().item(stack.kind);
    super::write_varint(i32::from(stack.count), output);
    super::write_varint(item_id, output);
    output.extend_from_slice(&[0, 0]); // Added and removed full component patches.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_item_vectors_map_menu_and_registry_ids() {
        // Hand-specified packet fields, excluding ID 0x39 and frame length.
        let cases: &[(&[u8], u8, BlockKind, u8)] = &[
            (&[0, 36, 64, 1, 0, 0], 0, BlockKind::Stone, 64),
            (&[0, 9, 17, 55, 0, 0], 9, BlockKind::Dirt, 17),
            (&[0, 44, 1, 54, 0, 0], 8, BlockKind::Grass, 1),
            (&[0, 35, 64, 87, 0, 0], 35, BlockKind::Bedrock, 64),
        ];
        for &(bytes, slot, kind, count) in cases {
            assert_eq!(
                creative(Fields(bytes)).unwrap(),
                Some(CreativeChange {
                    slot,
                    kind: Some(kind),
                    count,
                })
            );
        }
        assert_eq!(
            creative(Fields(&[0, 36, 0])).unwrap(),
            Some(CreativeChange {
                slot: 0,
                kind: None,
                count: 0,
            })
        );
        assert_eq!(
            creative(Fields(&[0, 35, 0])).unwrap(),
            Some(CreativeChange {
                slot: 35,
                kind: None,
                count: 0,
            })
        );
    }

    #[test]
    fn menu_indices_have_no_armor_crafting_offhand_or_drop_escape() {
        for menu in -1i16..=46 {
            let mut bytes = menu.to_be_bytes().to_vec();
            bytes.extend_from_slice(&[1, 1, 0, 0]);
            let change = creative(Fields(&bytes)).unwrap();
            match menu {
                9..=35 => assert_eq!(change.unwrap().slot, menu as u8),
                36..=44 => assert_eq!(change.unwrap().slot, (menu - 36) as u8),
                _ => assert_eq!(change, None),
            }
        }
    }

    #[test]
    fn unsupported_items_counts_and_patches_return_resynchronization() {
        for bytes in [
            vec![0, 36, 1, 0, 0, 0], // Air with a non-empty count.
            vec![0, 36, 1, 2, 0, 0], // Unimplemented ordinary item.
            vec![0, 36, 65, 1, 0, 0],
            vec![0, 36, 0xff, 0xff, 0xff, 0xff, 0x0f], // Negative count.
            vec![0, 36, 1, 0xff, 0xff, 0xff, 0xff, 0x0f, 0, 0], // Negative ID.
            vec![0, 36, 1, 1, 1, 0, 0xde, 0xad, 0xbe, 0xef], // Unknown added payload.
            vec![0, 36, 1, 1, 0, 1, 0x80, 0x01],       // Removed component.
        ] {
            assert_eq!(creative(Fields(&bytes)).unwrap(), None);
        }
    }

    #[test]
    fn truncation_and_trailing_supported_data_are_malformed() {
        let nonempty = [0, 36, 17, 55, 0, 0];
        for length in 0..nonempty.len() {
            assert!(
                creative(Fields(&nonempty[..length])).is_err(),
                "prefix {length}"
            );
        }
        for bytes in [&[0, 36, 0, 0][..], &[0, 36, 17, 55, 0, 0, 1][..]] {
            assert!(creative(Fields(bytes)).is_err());
        }
    }

    #[test]
    fn click_header_vectors_map_pickup_and_hotbar_swap() {
        for button in [0, 1] {
            assert_eq!(
                click(Fields(&[0, 7, 0, 36, button, 6, 0, 0])).unwrap(),
                Some(ClickChange {
                    state_id: 7,
                    click: Click::Collect {
                        slot: 0,
                        reverse: button == 1
                    }
                })
            );
        }
        for button in [0, 1] {
            assert_eq!(
                click(Fields(&[0, 7, 0, 36, button, 1, 0, 0])).unwrap(),
                Some(ClickChange {
                    state_id: 7,
                    click: Click::QuickMove { slot: 0 }
                })
            );
        }
        assert_eq!(
            click(Fields(&[0, 7, 0, 36, 0, 0, 0, 0])).unwrap(),
            Some(ClickChange {
                state_id: 7,
                click: Click::Pickup {
                    slot: 0,
                    right: false
                },
            })
        );
        assert_eq!(
            click(Fields(&[0, 1, 0, 9, 1, 0, 0, 0])).unwrap(),
            Some(ClickChange {
                state_id: 1,
                click: Click::Pickup {
                    slot: 9,
                    right: true
                },
            })
        );
        assert_eq!(
            click(Fields(&[0, 0, 0, 35, 8, 2, 0, 0])).unwrap(),
            Some(ClickChange {
                state_id: 0,
                click: Click::HotbarSwap {
                    slot: 35,
                    hotbar: 8
                },
            })
        );
        // Known but unsupported modes/menus/buttons/window and invalid state IDs.
        for bytes in [
            vec![0, 0, 0, 36, 2, 1, 0, 0],
            vec![0, 0, 0, 36, 2, 6, 0, 0],
            vec![0, 0, 0, 36, 2, 0, 0, 0],
            vec![0, 0, 0, 36, 9, 2, 0, 0],
            vec![0, 0, 0xff, 0xff, 0, 0, 0, 0],
            vec![0, 0, 0, 45, 0, 0, 0, 0],
            vec![1, 0, 0, 36, 0, 0, 0, 0],
            vec![0, 0x80, 0x80, 2, 0, 36, 0, 0, 0, 0],
        ] {
            assert_eq!(click(Fields(&bytes)).unwrap(), None);
        }
    }

    #[test]
    fn hashed_predictions_parse_added_values_before_removed_count_without_authority() {
        // Two changed keys, empty first stack, second stack with one added hash
        // and one removed component, followed by a different carried prediction.
        // These predicted counts are not used to construct the owner operation.
        let bytes = [
            0, 7, 0, 36, 0, 0, 2, 0, 36, 0, 0, 9, 1, 55, 17, 1, 5, 0xde, 0xad, 0xbe, 0xef, 1, 6, 1,
            1, 64, 0, 0,
        ];
        let change = click(Fields(&bytes)).unwrap().unwrap();
        assert_eq!(
            change,
            ClickChange {
                state_id: 7,
                click: Click::Pickup {
                    slot: 0,
                    right: false
                }
            }
        );
        let mut owner = sculith_game::inventory::Inventory::default();
        owner.give(sculith_world::BlockKind::Stone, 64).unwrap();
        owner.click(change.click, true).unwrap();
        assert_eq!(owner.cursor().unwrap().kind, BlockKind::Stone);
        assert_eq!(owner.cursor().unwrap().count, 64);
        assert_eq!(
            owner
                .get(sculith_game::inventory::Slot::Inventory(9))
                .unwrap(),
            None
        );
        for length in 0..bytes.len() {
            assert!(click(Fields(&bytes[..length])).is_err(), "prefix {length}");
        }
    }

    #[test]
    fn malformed_hashes_duplicates_limits_booleans_and_trailing_bytes_reject() {
        let header = [0, 7, 0, 36, 0, 0];
        let bodies: &[&[u8]] = &[
            &[2, 0, 9, 0, 0, 9, 0, 0],                         // Duplicate changed key.
            &[0x81, 1],                                        // More than 128 changed keys.
            &[0, 2],                                           // Noncanonical optional boolean.
            &[0, 1, 1, 1, 0x81, 2],                            // More than 256 added hashes.
            &[0, 1, 1, 1, 0, 0x81, 2],                         // More than 256 removed IDs.
            &[0, 1, 1, 1, 2, 3, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0], // Duplicate added ID.
            &[0, 1, 1, 1, 0, 2, 3, 3],                         // Duplicate removed ID.
            &[0, 1, 1, 1, 1, 3, 0, 0, 0, 0, 1, 3],             // Same added/removed ID.
            &[0, 0, 0],                                        // Trailing data after empty cursor.
        ];
        for body in bodies {
            let bytes = [header.as_slice(), body].concat();
            assert!(click(Fields(&bytes)).is_err(), "body {body:?}");
        }
        for body in [
            &[0, 1, 2, 1, 0, 0][..],  // Unsupported carried item.
            &[0, 1, 1, 0, 0, 0][..],  // Present stack with zero count.
            &[0, 1, 1, 65, 0, 0][..], // Excessive carried count.
            &[1, 0, 45, 0, 0][..],    // Unsupported changed menu key.
        ] {
            assert_eq!(
                click(Fields(&[header.as_slice(), body].concat())).unwrap(),
                None
            );
        }
    }

    #[test]
    fn authoritative_stack_encoding_has_actual_counts_and_validates_before_output() {
        let mut bytes = vec![];
        write_stack(None, &mut bytes).unwrap();
        assert_eq!(bytes, [0]);
        for (kind, id) in [
            (BlockKind::Stone, 1),
            (BlockKind::Dirt, 55),
            (BlockKind::Grass, 54),
            (BlockKind::Bedrock, 87),
        ] {
            bytes.clear();
            write_stack(
                Some(ItemStack {
                    kind,
                    count: 17,
                    net_id: 123,
                }),
                &mut bytes,
            )
            .unwrap();
            assert_eq!(bytes, [17, id, 0, 0]);
        }
        for stack in [
            ItemStack {
                kind: BlockKind::Air,
                count: 1,
                net_id: 1,
            },
            ItemStack {
                kind: BlockKind::Stone,
                count: 0,
                net_id: 1,
            },
            ItemStack {
                kind: BlockKind::Stone,
                count: 65,
                net_id: 1,
            },
            ItemStack {
                kind: BlockKind::Stone,
                count: 1,
                net_id: 0,
            },
        ] {
            let before = bytes.clone();
            assert!(write_stack(Some(stack), &mut bytes).is_err());
            assert_eq!(bytes, before);
        }
    }
}
#[test]
fn drag_buttons_require_ordered_owner_slots_and_the_outside_sentinel() {
    for (base, mode) in [(0, DragMode::Even), (4, DragMode::OneEach)] {
        for (stage, menu, expected) in [
            (0, -999i16, Click::DragStart { mode }),
            (1, 36, Click::DragAdd { slot: 0, mode }),
            (1, 9, Click::DragAdd { slot: 9, mode }),
            (2, -999, Click::DragFinish { mode }),
        ] {
            let mut bytes = vec![0, 7];
            bytes.extend(menu.to_be_bytes());
            bytes.extend([base + stage, 5, 0, 0]);
            assert_eq!(
                click(Fields(&bytes)).unwrap(),
                Some(ClickChange {
                    state_id: 7,
                    click: expected
                })
            );
        }
    }
    for (menu, button) in [
        (-999i16, 1),
        (36, 0),
        (36, 2),
        (-999, 3),
        (-999, 7),
        (-999, 8),
        (36, 9),
        (-999, 10),
        (-999, 255),
    ] {
        let mut bytes = vec![0, 0];
        bytes.extend(menu.to_be_bytes());
        bytes.extend([button, 5, 0, 0]);
        assert_eq!(click(Fields(&bytes)).unwrap(), None);
    }
}
