//! Native V2225 creative-builder inventory adaptation, without state ownership.
//!
//! Wire layouts: bedrock-rs 5221fcae8cc8468f523f3f58f129cb308e9d3369,
//! cross-checked with gophertunnel e2251a60f71d5faa9784d6a4bc817c117cb6af78.
//! Creative output slot 50 and ignored NumberOfCrafts boilerplate were checked
//! against Dragonfly 0f9a3a5736267db60bf46e92d408f81be1de9a64,
//! server/session/handler_{crafting,item_stack_request}.go. These are protocol
//! facts, independently implemented; official GDK inventory UI is not verified.

use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use bedrock::protocol::{
    V2225,
    v662::{enums::TextProcessingEventOrigin, packets::ItemStackResponsePacket},
    v712::types::ItemStackResponseContainerInfo,
    v729::types::FullContainerName,
    v2168::{
        enums::ItemStackNetResult,
        packets::{
            CreativeContentPacket, CreativeItemCategory, CreativeItemData, CreativeItemGroup,
        },
        types::{
            ItemStackRequestSlotInfo, NetworkItemInstanceDescriptor, NetworkItemStackDescriptorV2,
            RedactableString,
        },
    },
    v2193::types::{ItemStackResponseInfo, ItemStackResponseSlotInfo},
    v2225::enums::{ContainerEnumName, ItemStackRequestActionType as WireAction},
};
use sculith_world::BlockKind;

use sculith_game::inventory::{
    Action, CREATIVE_CATALOGUE, Change, INVENTORY_SLOTS, ItemStack, MAX_ACTIONS, MAX_STACK,
    Request, Response, Slot, SlotRef,
};

pub const MAX_REQUESTS: usize = 64;

/// Unsupported per-request semantics become an owner rejection with the same
/// ID. Errors here describe excessive packet envelopes or invalid owner state.
/// Movement/flag validation remains the input adapter's responsibility.
pub fn requests(packet: &V2225, selected_slot: u8) -> Result<Option<Vec<Request>>> {
    ensure!(selected_slot < 9, "invalid owner selected inventory slot");
    match packet {
        V2225::ItemStackRequestPacket(packet) => {
            ensure!(
                packet.requests.len() <= MAX_REQUESTS,
                "too many inventory requests"
            );
            Ok(Some(
                packet
                    .requests
                    .iter()
                    .map(|request| {
                        normalize(
                            request.client_request_id,
                            &request.actions,
                            &request.strings_to_filter,
                            &request.strings_to_filter_origin,
                            selected_slot,
                        )
                    })
                    .collect(),
            ))
        }
        V2225::PlayerAuthInputPacket(input) => {
            Ok(input.item_stack_request.as_ref().map(|request| {
                vec![normalize(
                    request.client_request_id,
                    &request.actions,
                    &request.strings_to_filter,
                    &request.strings_to_filter_origin,
                    selected_slot,
                )]
            }))
        }
        _ => Ok(None),
    }
}

fn normalize(
    id: i32,
    actions: &[WireAction<V2225>],
    strings: &[String],
    origin: &TextProcessingEventOrigin,
    selected_slot: u8,
) -> Request {
    let actions = if actions.len() > MAX_ACTIONS
        || !strings.is_empty()
        || matches!(origin, TextProcessingEventOrigin::Count)
    {
        // Never retain oversized action lists, strings, NBT or other wire data.
        vec![Action::Unsupported]
    } else {
        actions
            .iter()
            .map(|action| normalize_action(action, selected_slot).unwrap_or(Action::Unsupported))
            .collect()
    };
    Request { id, actions }
}

fn normalize_action(action: &WireAction<V2225>, selected_slot: u8) -> Option<Action> {
    match action {
        WireAction::Take {
            amount,
            source,
            destination,
        }
        | WireAction::Place {
            amount,
            source,
            destination,
        } => Some(Action::Transfer {
            source: slot_reference(source)?,
            destination: slot_reference(destination)?,
            count: count(*amount)?,
        }),
        WireAction::Swap {
            source,
            destination,
        } => Some(Action::Swap {
            source: slot_reference(source)?,
            destination: slot_reference(destination)?,
        }),
        WireAction::Destroy { amount, source } => Some(Action::Destroy {
            source: slot_reference(source)?,
            count: count(*amount)?,
        }),
        WireAction::CraftCreative {
            creative_item_network_id,
            ..
        } => {
            // Dragonfly ignores NumberOfCrafts here, generating one maximum
            // stack. The owner always creates 64, independent of this byte.
            Some(Action::CraftCreative {
                catalogue_id: *creative_item_network_id,
            })
        }
        WireAction::ScreenHUDMineBlock {
            hotbar_slot,
            predicted_durability,
            stack_network_id,
        } if *hotbar_slot == i32::from(selected_slot) && *predicted_durability == 0 => {
            Some(Action::Mine {
                source: SlotRef {
                    slot: Slot::Inventory(selected_slot),
                    expected_id: *stack_network_id,
                },
            })
        }
        // Drop needs actual dropped-item entities. Crafting, consumption and
        // the wire Create action need recipe validation; none are destruction.
        _ => None,
    }
}

fn count(amount: i8) -> Option<u8> {
    u8::try_from(amount)
        .ok()
        .filter(|count| (1..=MAX_STACK).contains(count))
}

fn slot_reference(reference: &ItemStackRequestSlotInfo<V2225>) -> Option<SlotRef> {
    if reference.container_name.dynamic_id.is_some() {
        return None;
    }
    let index = u8::try_from(reference.slot).ok()?;
    let slot = match reference.container_name.container {
        ContainerEnumName::HotbarContainer if index < 9 => Slot::Inventory(index),
        ContainerEnumName::InventoryContainer
        | ContainerEnumName::CombinedHotbarAndInventoryContainer
            if usize::from(index) < INVENTORY_SLOTS =>
        {
            Slot::Inventory(index)
        }
        ContainerEnumName::CursorContainer if index == 0 => Slot::Cursor,
        ContainerEnumName::CreatedOutputContainer if index == 50 => Slot::CreatedOutput,
        _ => return None,
    };
    Some(SlotRef {
        slot,
        expected_id: reference.raw_id,
    })
}

fn canonical_slot(slot: Slot) -> Result<(ContainerEnumName, i8)> {
    Ok(match slot {
        Slot::Inventory(index) => {
            ensure!(
                usize::from(index) < INVENTORY_SLOTS,
                "invalid owner inventory slot"
            );
            (ContainerEnumName::InventoryContainer, index as i8)
        }
        Slot::Cursor => (ContainerEnumName::CursorContainer, 0),
        Slot::CreatedOutput => (ContainerEnumName::CreatedOutputContainer, 50),
    })
}

/// Status errors have an explicit absent-container flag in the v2193 layout.
/// The session also sends current inventory/UI contents after a rejection.
pub fn response(response: &Response) -> Result<V2225> {
    let mut containers: BTreeMap<i8, ItemStackResponseContainerInfo<V2225>> = BTreeMap::new();
    if let Ok(changes) = &response.result {
        for &Change { slot, stack } in changes {
            if let Some(stack) = stack {
                validate_stack(stack)?;
            }
            let (container, slot) = canonical_slot(slot)?;
            let id = match container {
                ContainerEnumName::InventoryContainer => 29,
                ContainerEnumName::CursorContainer => 59,
                ContainerEnumName::CreatedOutputContainer => 60,
                _ => unreachable!("canonical owner container"),
            };
            containers
                .entry(id)
                .or_insert_with(|| ItemStackResponseContainerInfo {
                    container_name: FullContainerName {
                        container,
                        dynamic_id: None,
                    },
                    slots: vec![],
                })
                .slots
                .push(ItemStackResponseSlotInfo {
                    requested_slot: slot,
                    slot,
                    amount: stack.map_or(0, |stack| stack.count as i8),
                    item_stack_net_id: stack.map(|stack| stack.net_id),
                    custom_name: RedactableString {
                        unredacted: String::new(),
                        redacted: None,
                    },
                    durability_correction: 0,
                });
        }
    }
    Ok(V2225::ItemStackResponsePacket(Box::new(
        ItemStackResponsePacket {
            responses: vec![ItemStackResponseInfo {
                result: if response.result.is_ok() {
                    ItemStackNetResult::Success
                } else {
                    ItemStackNetResult::Error
                },
                client_request_id: response.request_id,
                containers: if containers.is_empty() {
                    None
                } else {
                    Some(containers.into_values().collect())
                },
            }],
        },
    )))
}

fn item_id(kind: BlockKind) -> Result<i16> {
    Ok(match kind {
        BlockKind::Stone => 1,
        BlockKind::Dirt => 3,
        BlockKind::Grass => 2,
        BlockKind::Bedrock => 7,
        BlockKind::Air => anyhow::bail!("air is an empty inventory slot"),
    })
}

fn validate_stack(stack: ItemStack) -> Result<()> {
    ensure!(
        (1..=MAX_STACK).contains(&stack.count),
        "invalid owner item count"
    );
    ensure!(stack.net_id > 0, "invalid owner stack ID");
    item_id(stack.kind)?;
    Ok(())
}

/// Full counted stack instances, including empty slots, use the V2225 layout.
pub fn stack(stack: Option<ItemStack>) -> Result<NetworkItemStackDescriptorV2> {
    if let Some(stack) = stack {
        validate_stack(stack)?;
    }
    Ok(NetworkItemStackDescriptorV2 {
        id: stack
            .map(|stack| item_id(stack.kind))
            .transpose()?
            .unwrap_or(0),
        stack_size: stack.map_or(0, |stack| u16::from(stack.count)),
        aux_value: 0,
        net_id: stack.map(|stack| stack.net_id),
        block_runtime_id: stack
            .map(|stack| super::chunk::block_hash(stack.kind))
            .transpose()?
            .unwrap_or(0),
        // Empty NBT marker (i16), zero can-place/can-break lengths (i32 each).
        user_data_buffer: if stack.is_some() { vec![0; 10] } else { vec![] },
    })
}

fn catalogue_stack(kind: BlockKind) -> Result<NetworkItemInstanceDescriptor> {
    Ok(NetworkItemInstanceDescriptor {
        id: i32::from(item_id(kind)?),
        stack_size: 1,
        aux_value: 0,
        block_runtime_id: super::chunk::block_hash(kind)? as i32,
        user_data_buffer: vec![0; 10],
    })
}

pub fn creative_catalogue() -> Result<V2225> {
    let contents = CREATIVE_CATALOGUE
        .iter()
        .copied()
        .enumerate()
        .map(|(index, kind)| {
            Ok(CreativeItemData {
                creative_net_id: index as u32 + 1,
                item_instance: catalogue_stack(kind)?,
                group_id: 0,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(V2225::CreativeContentPacket(Box::new(
        CreativeContentPacket {
            groups: vec![CreativeItemGroup {
                category: CreativeItemCategory::Construction,
                name: "itemGroup.name.stone".into(),
                icon: catalogue_stack(BlockKind::Stone)?,
            }],
            contents,
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bedrock::protocol::{
        PacketHeader, Packets, ProtoCodec,
        v662::packets::{ItemStackRequestPacket, RequestsEntry},
    };
    use sculith_game::inventory::{Inventory, RejectReason};
    use std::io::Cursor;

    fn wire_slot(
        container: ContainerEnumName,
        slot: i8,
        raw_id: i32,
    ) -> ItemStackRequestSlotInfo<V2225> {
        ItemStackRequestSlotInfo {
            container_name: FullContainerName {
                container,
                dynamic_id: None,
            },
            slot,
            raw_id,
        }
    }

    fn packet(id: i32, actions: Vec<WireAction<V2225>>) -> V2225 {
        V2225::ItemStackRequestPacket(Box::new(ItemStackRequestPacket {
            requests: vec![RequestsEntry {
                client_request_id: id,
                actions,
                strings_to_filter: vec![],
                strings_to_filter_origin: TextProcessingEventOrigin::Unknown,
            }],
        }))
    }

    fn roundtrip(packet: &V2225) -> V2225 {
        let mut bytes = Vec::new();
        packet
            .serialize(
                &PacketHeader {
                    packet_id: packet.id(),
                    ..Default::default()
                },
                &mut bytes,
            )
            .unwrap();
        let mut cursor = Cursor::new(bytes.as_slice());
        let (decoded, header) = V2225::deserialize(&mut cursor).unwrap();
        assert_eq!(header.packet_id, packet.id());
        assert_eq!(cursor.position() as usize, bytes.len());
        decoded
    }

    #[test]
    fn inventory_aliases_keep_indices_and_prediction_ids() {
        for container in [
            ContainerEnumName::HotbarContainer,
            ContainerEnumName::InventoryContainer,
            ContainerEnumName::CombinedHotbarAndInventoryContainer,
        ] {
            let wire = packet(
                -7,
                vec![WireAction::Take {
                    amount: 17,
                    source: wire_slot(container, 3, -6),
                    destination: wire_slot(ContainerEnumName::CursorContainer, 0, 0),
                }],
            );
            let request = requests(&roundtrip(&wire), 0).unwrap().unwrap().remove(0);
            assert_eq!(
                request,
                Request {
                    id: -7,
                    actions: vec![Action::Transfer {
                        source: SlotRef {
                            slot: Slot::Inventory(3),
                            expected_id: -6
                        },
                        destination: SlotRef {
                            slot: Slot::Cursor,
                            expected_id: 0
                        },
                        count: 17,
                    }]
                }
            );
        }
    }

    #[test]
    fn creative_creation_uses_output_prediction_and_ignores_craft_boilerplate() {
        let mut inventory = Inventory::default();
        for kind in [BlockKind::Stone, BlockKind::Dirt, BlockKind::Grass] {
            inventory.give(kind, 64).unwrap();
        }
        let wire = packet(
            -1,
            vec![
                WireAction::CraftCreative {
                    creative_item_network_id: 4,
                    number_of_requested_crafts: 17,
                },
                WireAction::Place {
                    amount: 64,
                    source: wire_slot(ContainerEnumName::CreatedOutputContainer, 50, -1),
                    destination: wire_slot(ContainerEnumName::InventoryContainer, 9, 0),
                },
            ],
        );
        let request = requests(&roundtrip(&wire), 0).unwrap().unwrap().remove(0);
        assert!(inventory.apply(&request, true).result.is_ok());
        let created = inventory.get(Slot::Inventory(9)).unwrap().unwrap();
        assert_eq!((created.kind, created.count), (BlockKind::Bedrock, 64));
        assert_eq!(inventory.created_output(), None);
    }

    #[test]
    fn embedded_mining_and_standalone_requests_use_the_same_normalizer() {
        use bedrock::protocol::{
            v662::enums::InputMode,
            v2168::enums::{NewInteractionModel, PlayerAuthInputData},
            v2193::packets::{
                ClientPlayMode, PlayerAuthInputPacket,
                player_auth_input_packet::PerformItemStackRequestData,
            },
        };
        let standalone = packet(
            -7,
            vec![WireAction::ScreenHUDMineBlock {
                hotbar_slot: 2,
                predicted_durability: 0,
                stack_network_id: 3,
            }],
        );
        let V2225::ItemStackRequestPacket(outer) = &standalone else {
            unreachable!()
        };
        let request = &outer.requests[0];
        let embedded = V2225::PlayerAuthInputPacket(Box::new(PlayerAuthInputPacket {
            player_rotation: (0.0, 0.0),
            player_position: (0.5, 5.62, 0.5),
            move_vector: (0.0, 0.0),
            player_head_rotation: 0.0,
            input_data: vec![PlayerAuthInputData::PerformItemStackRequest],
            input_mode: InputMode::Mouse,
            play_mode: ClientPlayMode::Normal,
            new_interaction_model: NewInteractionModel::Crosshair,
            interact_rotation: (0.0, 0.0),
            client_tick: 99,
            pos_delta: (0.0, 0.0, 0.0),
            item_use_transaction: None,
            item_stack_request: Some(PerformItemStackRequestData {
                client_request_id: request.client_request_id,
                actions: request.actions.clone(),
                strings_to_filter: request.strings_to_filter.clone(),
                strings_to_filter_origin: TextProcessingEventOrigin::Unknown,
            }),
            player_block_actions: None,
            vehicle_rotation: None,
            client_predicted_vehicle: None,
            analog_move_vector: (0.0, 0.0),
            camera_orientation: (0.0, 0.0, 1.0),
            raw_move_vector: (0.0, 0.0),
        }));
        let normalized = requests(&roundtrip(&embedded), 2).unwrap().unwrap();
        assert_eq!(Some(normalized.clone()), requests(&standalone, 2).unwrap());
        let mut inventory = Inventory::default();
        for kind in [BlockKind::Stone, BlockKind::Dirt, BlockKind::Grass] {
            inventory.give(kind, 64).unwrap();
        }
        let before = inventory.clone();
        let response = inventory.apply(&normalized[0], true);
        assert_eq!(response.result.unwrap().len(), 1);
        assert_eq!(inventory.slots(), before.slots());
        assert_eq!(inventory.cursor(), before.cursor());
        assert_eq!(
            requests(&embedded, 0).unwrap().unwrap()[0].actions,
            vec![Action::Unsupported]
        );
    }

    #[test]
    fn unsupported_ordinary_requests_reject_without_mutation_or_parser_error() {
        let mut dynamic = wire_slot(ContainerEnumName::InventoryContainer, 0, 1);
        dynamic.container_name.dynamic_id = Some(0);
        let actions = vec![
            WireAction::Drop {
                amount: 1,
                source: wire_slot(ContainerEnumName::InventoryContainer, 0, 1),
                randomly: false,
            },
            WireAction::Create { slot: 0 },
            WireAction::Consume {
                amount: 1,
                source: wire_slot(ContainerEnumName::InventoryContainer, 0, 1),
            },
            WireAction::Destroy {
                amount: 0,
                source: wire_slot(ContainerEnumName::InventoryContainer, 0, 1),
            },
            WireAction::Destroy {
                amount: -1,
                source: wire_slot(ContainerEnumName::InventoryContainer, 0, 1),
            },
            WireAction::Destroy {
                amount: 65,
                source: wire_slot(ContainerEnumName::InventoryContainer, 0, 1),
            },
            WireAction::Destroy {
                amount: 1,
                source: dynamic,
            },
            WireAction::Destroy {
                amount: 1,
                source: wire_slot(ContainerEnumName::ArmorContainer, 0, 1),
            },
            WireAction::Destroy {
                amount: 1,
                source: wire_slot(ContainerEnumName::InventoryContainer, 36, 0),
            },
            WireAction::Destroy {
                amount: 1,
                source: wire_slot(ContainerEnumName::HotbarContainer, 9, 0),
            },
            WireAction::Destroy {
                amount: 1,
                source: wire_slot(ContainerEnumName::CursorContainer, 1, 0),
            },
            WireAction::Destroy {
                amount: 1,
                source: wire_slot(ContainerEnumName::CreatedOutputContainer, 0, 0),
            },
            WireAction::ScreenHUDMineBlock {
                hotbar_slot: 1,
                predicted_durability: 0,
                stack_network_id: 2,
            },
        ];
        for action in actions {
            let request = requests(&packet(-1, vec![action]), 0)
                .unwrap()
                .unwrap()
                .remove(0);
            let mut inventory = Inventory::default();
            for kind in [BlockKind::Stone, BlockKind::Dirt, BlockKind::Grass] {
                inventory.give(kind, 64).unwrap();
            }
            let before = *inventory.slots();
            assert_eq!(
                inventory.apply(&request, true).result,
                Err(RejectReason::UnsupportedAction)
            );
            assert_eq!(*inventory.slots(), before);
        }
    }

    #[test]
    fn field_and_action_limits_produce_bounded_owner_rejections() {
        let too_many = packet(
            -1,
            (0..=MAX_ACTIONS)
                .map(|_| WireAction::CraftCreative {
                    creative_item_network_id: 1,
                    number_of_requested_crafts: 1,
                })
                .collect(),
        );
        assert_eq!(
            requests(&too_many, 0).unwrap().unwrap()[0].actions,
            vec![Action::Unsupported]
        );
        let mut filtered = packet(-2, vec![]);
        let V2225::ItemStackRequestPacket(request) = &mut filtered else {
            unreachable!()
        };
        request.requests[0]
            .strings_to_filter
            .push("unsupported naming".into());
        assert_eq!(
            requests(&filtered, 0).unwrap().unwrap()[0].actions,
            vec![Action::Unsupported]
        );
        assert!(requests(&filtered, 9).is_err());
    }

    #[test]
    fn response_matches_pinned_go_optional_container_and_stack_id_layout() {
        let reply = response(&Response {
            request_id: -7,
            result: Ok(vec![
                Change {
                    slot: Slot::Inventory(0),
                    stack: Some(ItemStack {
                        kind: BlockKind::Stone,
                        count: 63,
                        net_id: 5,
                    }),
                },
                Change {
                    slot: Slot::Cursor,
                    stack: Some(ItemStack {
                        kind: BlockKind::Stone,
                        count: 1,
                        net_id: 6,
                    }),
                },
            ]),
            replayed: false,
        })
        .unwrap();
        let mut bytes = Vec::new();
        reply
            .serialize(
                &PacketHeader {
                    packet_id: 148,
                    ..Default::default()
                },
                &mut bytes,
            )
            .unwrap();
        // Independently specified from gophertunnel's ItemStackResponse.Marshal,
        // StackResponseSlotInfo.Marshal and FullContainerName.Marshal.
        assert_eq!(
            bytes,
            [
                0x94, 1, 1, 0, 13, 1, 2, 29, 0, 1, 0, 0, 63, 1, 10, 0, 0, 0, 59, 0, 1, 0, 0, 1, 1,
                12, 0, 0, 0,
            ]
        );
        let V2225::ItemStackResponsePacket(decoded) = roundtrip(&reply) else {
            unreachable!()
        };
        let containers = decoded.responses[0].containers.as_ref().unwrap();
        assert_eq!(containers.len(), 2);
        assert_eq!(containers[0].slots[0].item_stack_net_id, Some(5));
        assert_eq!(containers[1].slots[0].amount, 1);

        let rejection = response(&Response {
            request_id: -7,
            result: Err(RejectReason::UnsupportedAction),
            replayed: false,
        })
        .unwrap();
        bytes.clear();
        rejection
            .serialize(
                &PacketHeader {
                    packet_id: 148,
                    ..Default::default()
                },
                &mut bytes,
            )
            .unwrap();
        assert_eq!(bytes, [0x94, 1, 1, 1, 13, 0]);
        roundtrip(&rejection);

        let cleared = response(&Response {
            request_id: -1,
            result: Ok(vec![Change {
                slot: Slot::CreatedOutput,
                stack: None,
            }]),
            replayed: false,
        })
        .unwrap();
        bytes.clear();
        cleared
            .serialize(
                &PacketHeader {
                    packet_id: 148,
                    ..Default::default()
                },
                &mut bytes,
            )
            .unwrap();
        assert_eq!(
            bytes,
            [0x94, 1, 1, 0, 1, 1, 1, 60, 0, 1, 50, 50, 0, 0, 0, 0, 0]
        );
        let V2225::ItemStackResponsePacket(decoded) = roundtrip(&cleared) else {
            unreachable!()
        };
        let slots = &decoded.responses[0].containers.as_ref().unwrap()[0].slots;
        assert_eq!((slots[0].amount, slots[0].item_stack_net_id), (0, None));
    }

    #[test]
    fn catalogue_and_counted_stack_ids_use_distinct_namespaces() {
        let V2225::CreativeContentPacket(catalogue) = roundtrip(&creative_catalogue().unwrap())
        else {
            unreachable!()
        };
        assert_eq!(catalogue.groups.len(), 1);
        assert_eq!(
            catalogue
                .contents
                .iter()
                .map(|entry| entry.creative_net_id)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert_eq!(
            catalogue
                .contents
                .iter()
                .map(|entry| entry.item_instance.id)
                .collect::<Vec<_>>(),
            [1, 3, 2, 7]
        );
        assert!(
            catalogue
                .contents
                .iter()
                .all(|entry| entry.group_id == 0 && entry.item_instance.stack_size == 1)
        );
        let encoded = stack(Some(ItemStack {
            kind: BlockKind::Dirt,
            count: 17,
            net_id: 1234,
        }))
        .unwrap();
        assert_eq!(
            (encoded.id, encoded.stack_size, encoded.net_id),
            (3, 17, Some(1234))
        );
        assert_eq!(
            encoded.block_runtime_id,
            super::super::chunk::block_hash(BlockKind::Dirt).unwrap()
        );
        let mut bytes = vec![];
        encoded.serialize(&mut bytes).unwrap();
        let decoded = NetworkItemStackDescriptorV2::deserialize(&mut bytes.as_slice()).unwrap();
        assert_eq!((decoded.stack_size, decoded.net_id), (17, Some(1234)));
        assert_eq!(stack(None).unwrap().net_id, None);
    }
}
