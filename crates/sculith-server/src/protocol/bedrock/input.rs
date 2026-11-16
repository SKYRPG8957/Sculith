//! V2225 wire requests translated to small, untrusted intent values.
//!
//! Packet layouts come from Apache-2.0 bedrock-rs commit
//! 5221fcae8cc8468f523f3f58f129cb308e9d3369 (generated V2225, v2193
//! PlayerAuthInput/InventoryTransaction and v662 PlayerAction).
//! Action meanings and the 1.62 eye/base offset were cross-checked against
//! MIT Dragonfly commit 0f9a3a5736267db60bf46e92d408f81be1de9a64,
//! server/session/handler_{player_auth_input,player_action,inventory_transaction}.go.
//! This independently implements protocol facts; it does not copy their handlers.
//!
//! Decoding never grants permission, applies a client stack, checks reach, or
//! commits a position. The world owner must verify its own game mode, inventory,
//! collision, rate and ownership. Move.position is the raw wire eye/base position:
//! the adapter must subtract 1.62 from Y before submitting a feet position.

use anyhow::{Result, bail, ensure};
use bedrock::protocol::{
    V2225,
    v662::{
        enums::{ContainerID, InputMode, PlayerActionType},
        types::InventoryTransactionData,
    },
    v2168::{
        enums::{NewInteractionModel, PlayerAuthInputData},
        types::NetworkItemStackDescriptorV2,
    },
    v2193::{
        packets::{ClientPlayMode, PlayerAuthInputPacket},
        types::ItemUseTransactionData,
    },
};
use sculith_world::{BlockPos, MAX_Y, MIN_Y, WORLD_BORDER};

const MAX_BLOCK_ACTIONS: usize = 32;
const MAX_INPUT_FLAGS: usize = 66;
const MAX_ITEM_USER_BYTES: usize = 8192;

#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    /// Raw eye/base position; convert Y to feet before world validation.
    Move {
        tick: u64,
        position: (f32, f32, f32),
        /// Pitch followed by normalized yaw, both degrees.
        rotation: (f32, f32),
        head_yaw: f32,
    },
    /// A completion request, not proof that mining is legal or finished.
    Break { position: BlockPos },
    /// All item information is a claim; select a block from server inventory.
    Place {
        against: BlockPos,
        face: u8,
        hotbar_slot: u8,
        claimed_item_network_id: i32,
    },
    /// Select an existing server hotbar slot; never replace its contents.
    SelectSlot { slot: u8 },
}

/// Fully validate a packet before returning any intentions, preserving its order.
/// Unsupported inventory mutations return an error instead of being accepted.
pub fn decode(packet: &V2225, expected_entity_id: u64) -> Result<Vec<Intent>> {
    ensure!(
        expected_entity_id != 0,
        "input requires an initialized entity"
    );
    match packet {
        V2225::PlayerAuthInputPacket(input) => auth_input(input),
        V2225::PlayerActionPacket(action) => {
            ensure!(
                action.player_runtime_id.0 == expected_entity_id,
                "player action targets another entity"
            );
            let position = BlockPos {
                x: action.block_position.x,
                y: action.block_position.y,
                z: action.block_position.z,
            };
            let mut intents = Vec::new();
            block_action(&action.action, position, action.face, false, &mut intents)?;
            Ok(intents)
        }
        V2225::InventoryTransactionPacket(transaction) => {
            ensure!(
                transaction.legacy_request_id == 0 && transaction.legacy_set_item_slots.is_none(),
                "unsupported legacy inventory slot mutation/resync request"
            );
            ensure!(
                transaction.actions.is_empty(),
                "unsupported inventory mutation action list"
            );
            match &transaction.data {
                InventoryTransactionData::ItemUse(data) => Ok(vec![item_use(data)?]),
                _ => bail!("unsupported inventory transaction type"),
            }
        }
        V2225::MobEquipmentPacket(equipment) => {
            ensure!(
                equipment.target_runtime_id.0 == expected_entity_id,
                "equipment targets another entity"
            );
            ensure!(
                equipment.container_id == ContainerID::INVENTORY,
                "unsupported equipment container"
            );
            let slot = hotbar(i32::from(equipment.slot))?;
            ensure!(
                slot == hotbar(i32::from(equipment.selected_slot))?,
                "equipment hotbar slots disagree"
            );
            validate_item(&equipment.item)?;
            Ok(vec![Intent::SelectSlot { slot }])
        }
        V2225::PlayerHotbarPacket(hotbar_packet) => {
            ensure!(
                hotbar_packet.container_id == ContainerID::INVENTORY,
                "unsupported hotbar container"
            );
            ensure!(
                hotbar_packet.selected_slot <= 8,
                "hotbar slot must be 0..=8"
            );
            Ok(if hotbar_packet.should_select_slot {
                vec![Intent::SelectSlot {
                    slot: hotbar_packet.selected_slot as u8,
                }]
            } else {
                vec![]
            })
        }
        V2225::ItemStackRequestPacket(_) => {
            bail!("unsupported item-stack inventory mutation request")
        }
        V2225::AnimatePacket(animation) => {
            ensure!(
                animation.target_runtime_id.0 == expected_entity_id,
                "animation targets another entity"
            );
            ensure!(animation.data.is_finite(), "non-finite animation data");
            ensure!(animation.hand <= 1, "invalid animation hand");
            Ok(vec![])
        }
        // Keepalive, loading state and other packets have separate handlers.
        // In particular no animation, cache or server-bound status is a block edit.
        _ => Ok(vec![]),
    }
}

fn auth_input(input: &PlayerAuthInputPacket<V2225>) -> Result<Vec<Intent>> {
    finite_position(input.player_position)?;
    ensure!(
        input.player_rotation.0.is_finite() && (-90.0..=90.0).contains(&input.player_rotation.0),
        "invalid player pitch"
    );
    let yaw = angle(input.player_rotation.1)?;
    let head_yaw = angle(input.player_head_rotation)?;
    for value in [
        input.move_vector.0,
        input.move_vector.1,
        input.interact_rotation.0,
        input.interact_rotation.1,
        input.pos_delta.0,
        input.pos_delta.1,
        input.pos_delta.2,
        input.analog_move_vector.0,
        input.analog_move_vector.1,
        input.camera_orientation.0,
        input.camera_orientation.1,
        input.camera_orientation.2,
        input.raw_move_vector.0,
        input.raw_move_vector.1,
    ] {
        ensure!(value.is_finite(), "non-finite auxiliary movement input");
    }
    ensure!(
        input.input_data.len() <= MAX_INPUT_FLAGS,
        "too many movement flags"
    );
    for (index, flag) in input.input_data.iter().enumerate() {
        ensure!(
            !input.input_data[..index].contains(flag),
            "duplicate movement flag"
        );
    }
    ensure!(
        !matches!(input.input_mode, InputMode::Undefined | InputMode::Count),
        "unsupported input mode"
    );
    // ClientPlayMode describes the client's presentation (for example Screen),
    // not the authoritative creative/survival game mode. Real Windows GDK
    // clients can send non-Normal values immediately after the spawn ack.
    // All concrete protocol modes still pass the same owner validation below.
    ensure!(
        !matches!(input.play_mode, ClientPlayMode::NumModes),
        "invalid client play mode sentinel"
    );
    ensure!(
        !matches!(input.new_interaction_model, NewInteractionModel::Count),
        "invalid interaction model"
    );
    let flag = |needle| input.input_data.contains(&needle);
    ensure!(
        flag(PlayerAuthInputData::PerformBlockActions) == input.player_block_actions.is_some(),
        "block-action flag and payload disagree"
    );
    ensure!(
        flag(PlayerAuthInputData::PerformItemInteraction) == input.item_use_transaction.is_some(),
        "item-interaction flag and payload disagree"
    );
    ensure!(
        flag(PlayerAuthInputData::PerformItemStackRequest) == input.item_stack_request.is_some(),
        "item-stack flag and payload disagree"
    );
    ensure!(
        input.item_stack_request.is_none(),
        "unsupported embedded item-stack inventory mutation request"
    );
    ensure!(
        !flag(PlayerAuthInputData::IsInClientPredictedVehicle)
            && input.vehicle_rotation.is_none()
            && input.client_predicted_vehicle.is_none(),
        "client-predicted vehicles are unsupported"
    );
    if let Some(actions) = &input.player_block_actions {
        ensure!(actions.len() <= MAX_BLOCK_ACTIONS, "too many block actions");
    }
    let mut intents =
        Vec::with_capacity(2 + input.player_block_actions.as_ref().map_or(0, Vec::len));
    intents.push(Intent::Move {
        tick: input.client_tick,
        position: input.player_position,
        rotation: (input.player_rotation.0, yaw),
        head_yaw,
    });
    // A packet's interaction comes before its ordered block-action sequence.
    if let Some(transaction) = &input.item_use_transaction {
        ensure!(
            transaction.legacy_request_id == 0 && transaction.legacy_set_item_slots.is_none(),
            "unsupported embedded legacy inventory mutation/resync request"
        );
        ensure!(
            transaction.actions.action.is_empty(),
            "unsupported embedded inventory mutation action list"
        );
        intents.push(item_use(&transaction.item_use)?);
    }
    if let Some(actions) = &input.player_block_actions {
        for action in actions {
            block_action(
                &action.action_type,
                BlockPos {
                    x: action.position.x,
                    y: action.position.y,
                    z: action.position.z,
                },
                action.facing,
                true,
                &mut intents,
            )?;
        }
    }
    Ok(intents)
}

fn block_action(
    action: &PlayerActionType,
    position: BlockPos,
    facing: i32,
    embedded: bool,
    intents: &mut Vec<Intent>,
) -> Result<()> {
    match action {
        PlayerActionType::PredictDestroyBlock => {
            validate_block(position)?;
            face(facing)?;
            intents.push(Intent::Break { position });
        }
        PlayerActionType::StartDestroyBlock
        | PlayerActionType::AbortDestroyBlock
        | PlayerActionType::StopDestroyBlock
        | PlayerActionType::ContinueDestroyBlock
        | PlayerActionType::CrackBlock
        | PlayerActionType::GetUpdatedBlock
        | PlayerActionType::DenyDestroyBlock
        | PlayerActionType::CreativeDestroyBlock => {
            validate_block(position)?;
            ensure!((-1..=5).contains(&facing), "invalid block-action face");
            // Start/abort/stop/progress and the redundant creative notification
            // are not the authoritative ItemUse Destroy or PredictDestroy request.
            // In particular a cancelled animation must never delete a block.
        }
        PlayerActionType::StartJump
        | PlayerActionType::StartSprinting
        | PlayerActionType::StopSprinting
        | PlayerActionType::StartSneaking
        | PlayerActionType::StopSneaking
        | PlayerActionType::StartSwimming
        | PlayerActionType::StopSwimming
        | PlayerActionType::StartGliding
        | PlayerActionType::StopGliding
        | PlayerActionType::StartCrawling
        | PlayerActionType::StopCrawling
        | PlayerActionType::StartFlying
        | PlayerActionType::StopFlying
        | PlayerActionType::HandledTeleport
        | PlayerActionType::ClientAckServerData
        | PlayerActionType::MissedSwing
            if !embedded => {}
        _ => bail!("unsupported player/block action"),
    }
    Ok(())
}

fn item_use(data: &ItemUseTransactionData<V2225>) -> Result<Intent> {
    let position = BlockPos {
        x: data.block_position.x,
        y: data.block_position.y,
        z: data.block_position.z,
    };
    validate_block(position)?;
    let slot = hotbar(data.hotbar_slot)?;
    ensure!(data.hand == 0, "offhand item interaction is unsupported");
    validate_item(&data.held_item)?;
    finite_position(data.player_position)?;
    for value in [
        data.click_position.0,
        data.click_position.1,
        data.click_position.2,
    ] {
        ensure!(
            value.is_finite() && (-0.001..=1.001).contains(&value),
            "invalid local block click position"
        );
    }
    let face = face(i32::from(data.block_face))?;
    match data.action_type {
        0 => {
            ensure!(
                data.held_item.id != 0,
                "cannot request placement with an empty item"
            );
            Ok(Intent::Place {
                against: position,
                face,
                hotbar_slot: slot,
                claimed_item_network_id: i32::from(data.held_item.id),
            })
        }
        2 => Ok(Intent::Break { position }),
        _ => bail!("unsupported item-use action"),
    }
}

fn validate_item(item: &NetworkItemStackDescriptorV2) -> Result<()> {
    ensure!(
        item.user_data_buffer.len() <= MAX_ITEM_USER_BYTES,
        "item user data exceeds 8 KiB"
    );
    ensure!(
        (item.id == 0 && item.stack_size == 0)
            || (item.id != 0 && (1..=64).contains(&item.stack_size)),
        "invalid claimed item count"
    );
    ensure!(
        item.net_id.is_none_or(|id| id >= 0),
        "invalid item stack network ID"
    );
    // aux/NBT/count/net ID are never copied into the authoritative inventory.
    Ok(())
}

fn finite_position(position: (f32, f32, f32)) -> Result<()> {
    ensure!(
        [position.0, position.1, position.2]
            .into_iter()
            .all(f32::is_finite),
        "non-finite player position"
    );
    ensure!(
        position.0.abs() < WORLD_BORDER as f32 && position.2.abs() < WORLD_BORDER as f32,
        "position outside horizontal world bounds"
    );
    // This is only a coarse envelope. The owner checks feet Y and collision.
    ensure!(
        ((MIN_Y - 128) as f32..=(MAX_Y + 128) as f32).contains(&position.1),
        "position outside vertical input bounds"
    );
    Ok(())
}

fn angle(value: f32) -> Result<f32> {
    ensure!(
        value.is_finite() && value.abs() <= 360_000.0,
        "invalid yaw angle"
    );
    Ok((value + 180.0).rem_euclid(360.0) - 180.0)
}

fn validate_block(position: BlockPos) -> Result<()> {
    ensure!(
        (-WORLD_BORDER..WORLD_BORDER).contains(&position.x)
            && (-WORLD_BORDER..WORLD_BORDER).contains(&position.z)
            && (MIN_Y..=MAX_Y).contains(&position.y),
        "block position outside world bounds"
    );
    Ok(())
}

fn face(value: i32) -> Result<u8> {
    ensure!((0..=5).contains(&value), "block face must be 0..=5");
    Ok(value as u8)
}

fn hotbar(value: i32) -> Result<u8> {
    ensure!((0..=8).contains(&value), "hotbar slot must be 0..=8");
    Ok(value as u8)
}

#[cfg(test)]
pub(in crate::protocol::bedrock) mod tests {
    use super::*;
    use std::io::Cursor;

    use bedrock::protocol::{
        PacketHeader, Packets,
        v662::{
            packets::{NetworkStackLatencyPacket, PlayerActionPacket, PlayerHotbarPacket},
            types::{ActorRuntimeID, BlockPos as WireBlockPos},
        },
        v944::types::NetworkBlockPosition,
        v975::packets::MobEquipmentPacket,
        v2168::types::PlayerBlockActionData,
        v2193::packets::InventoryTransactionPacket,
    };

    pub(in crate::protocol::bedrock) fn movement() -> PlayerAuthInputPacket<V2225> {
        PlayerAuthInputPacket {
            player_rotation: (20.0, 450.0),
            player_position: (-0.5, 5.62, 0.5),
            move_vector: (0.0, 0.0),
            player_head_rotation: -450.0,
            input_data: vec![],
            input_mode: InputMode::Mouse,
            play_mode: ClientPlayMode::Normal,
            new_interaction_model: NewInteractionModel::Crosshair,
            interact_rotation: (0.0, 0.0),
            client_tick: 99,
            pos_delta: (0.0, 0.0, 0.0),
            item_use_transaction: None,
            item_stack_request: None,
            player_block_actions: None,
            vehicle_rotation: None,
            client_predicted_vehicle: None,
            analog_move_vector: (0.0, 0.0),
            camera_orientation: (0.0, 0.0, 1.0),
            raw_move_vector: (0.0, 0.0),
        }
    }

    fn item() -> NetworkItemStackDescriptorV2 {
        NetworkItemStackDescriptorV2 {
            id: 1,
            stack_size: 64,
            aux_value: 0,
            net_id: Some(1),
            block_runtime_id: 0,
            user_data_buffer: vec![],
        }
    }

    fn item_use_request(action_type: i32) -> ItemUseTransactionData<V2225> {
        ItemUseTransactionData {
            action_type,
            trigger_type: 1,
            block_position: NetworkBlockPosition { x: -1, y: 3, z: 0 },
            block_face: 1,
            hotbar_slot: 0,
            hand: 0,
            held_item: item(),
            player_position: (-0.5, 5.62, 0.5),
            click_position: (0.5, 1.0, 0.5),
            block_runtime_id: 0,
            client_prediction: 0,
            client_cooldown_state: 0,
        }
    }

    pub(in crate::protocol::bedrock) fn transaction(action_type: i32) -> V2225 {
        V2225::InventoryTransactionPacket(Box::new(InventoryTransactionPacket {
            legacy_request_id: 0,
            legacy_set_item_slots: None,
            actions: vec![],
            data: InventoryTransactionData::ItemUse(item_use_request(action_type)),
        }))
    }

    fn action(kind: PlayerActionType, actor: u64) -> V2225 {
        V2225::PlayerActionPacket(Box::new(PlayerActionPacket {
            player_runtime_id: ActorRuntimeID(actor),
            action: kind,
            block_position: NetworkBlockPosition { x: -1, y: 3, z: 0 },
            result_pos: NetworkBlockPosition { x: -1, y: 3, z: 0 },
            face: 1,
        }))
    }

    fn roundtrip(packet: &V2225) -> V2225 {
        let header = PacketHeader {
            packet_id: packet.id(),
            ..PacketHeader::default()
        };
        let mut bytes = Vec::new();
        packet.serialize(&header, &mut bytes).unwrap();
        let mut cursor = Cursor::new(bytes.as_slice());
        let (decoded, actual_header) = V2225::deserialize(&mut cursor).unwrap();
        assert_eq!(actual_header.packet_id, header.packet_id);
        assert_eq!(cursor.position() as usize, bytes.len());
        decoded
    }

    #[test]
    fn newest_movement_codec_keeps_raw_eye_position_and_normalizes_yaw() {
        let packet = V2225::PlayerAuthInputPacket(Box::new(movement()));
        let decoded = decode(&roundtrip(&packet), 1).unwrap();
        assert_eq!(
            decoded,
            [Intent::Move {
                tick: 99,
                position: (-0.5, 5.62, 0.5),
                rotation: (20.0, 90.0),
                head_yaw: -90.0
            }]
        );
    }

    #[test]
    fn client_presentation_modes_do_not_change_authoritative_movement_intents() {
        let expected = decode(&V2225::PlayerAuthInputPacket(Box::new(movement())), 1).unwrap();
        for mode in [
            ClientPlayMode::Normal,
            ClientPlayMode::Teaser,
            ClientPlayMode::Screen,
            ClientPlayMode::Viewer,
            ClientPlayMode::Reality,
            ClientPlayMode::Placement,
            ClientPlayMode::LivingRoom,
            ClientPlayMode::ExitLevel,
            ClientPlayMode::ExitLevelLivingRoom,
        ] {
            let mut input = movement();
            input.play_mode = mode;
            assert_eq!(
                decode(
                    &roundtrip(&V2225::PlayerAuthInputPacket(Box::new(input))),
                    1
                )
                .unwrap(),
                expected
            );
        }
        let mut input = movement();
        input.play_mode = ClientPlayMode::NumModes;
        assert!(decode(&V2225::PlayerAuthInputPacket(Box::new(input)), 1).is_err());
    }

    #[test]
    fn movement_rejects_nonfinite_bounds_rotation_and_ambiguous_flags() {
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 30_000_000.0] {
            let mut input = movement();
            input.player_position.0 = value;
            assert!(decode(&V2225::PlayerAuthInputPacket(Box::new(input)), 1).is_err());
        }
        for pitch in [-90.1, 90.1, f32::NAN] {
            let mut input = movement();
            input.player_rotation.0 = pitch;
            assert!(decode(&V2225::PlayerAuthInputPacket(Box::new(input)), 1).is_err());
        }
        let mut input = movement();
        input.raw_move_vector.0 = f32::NAN;
        assert!(decode(&V2225::PlayerAuthInputPacket(Box::new(input)), 1).is_err());
        let mut input = movement();
        input.input_data = vec![PlayerAuthInputData::Sneaking; 2];
        assert!(decode(&V2225::PlayerAuthInputPacket(Box::new(input)), 1).is_err());
        let mut input = movement();
        input.input_data = vec![PlayerAuthInputData::PerformBlockActions];
        assert!(decode(&V2225::PlayerAuthInputPacket(Box::new(input)), 1).is_err());
    }

    #[test]
    fn start_abort_stop_and_creative_notifications_never_break_a_block() {
        for kind in [
            PlayerActionType::StartDestroyBlock,
            PlayerActionType::AbortDestroyBlock,
            PlayerActionType::StopDestroyBlock,
            PlayerActionType::ContinueDestroyBlock,
            PlayerActionType::CrackBlock,
            PlayerActionType::CreativeDestroyBlock,
        ] {
            assert!(decode(&roundtrip(&action(kind, 1)), 1).unwrap().is_empty());
        }
        assert_eq!(
            decode(
                &roundtrip(&action(PlayerActionType::PredictDestroyBlock, 1)),
                1
            )
            .unwrap(),
            [Intent::Break {
                position: BlockPos { x: -1, y: 3, z: 0 }
            }]
        );
        assert!(decode(&action(PlayerActionType::PredictDestroyBlock, 2), 1).is_err());
        let V2225::PlayerActionPacket(mut forged) = action(PlayerActionType::StartDestroyBlock, 1)
        else {
            unreachable!()
        };
        forged.face = 6;
        assert!(decode(&V2225::PlayerActionPacket(forged), 1).is_err());
    }

    #[test]
    fn embedded_block_actions_preserve_order_and_are_bounded() {
        let mut input = movement();
        input
            .input_data
            .push(PlayerAuthInputData::PerformBlockActions);
        input.player_block_actions = Some(vec![
            PlayerBlockActionData {
                action_type: PlayerActionType::StartDestroyBlock,
                position: WireBlockPos { x: -1, y: 3, z: 0 },
                facing: 1,
            },
            PlayerBlockActionData {
                action_type: PlayerActionType::PredictDestroyBlock,
                position: WireBlockPos { x: -1, y: 3, z: 0 },
                facing: 1,
            },
        ]);
        let result = decode(
            &roundtrip(&V2225::PlayerAuthInputPacket(Box::new(input.clone()))),
            1,
        )
        .unwrap();
        assert!(matches!(result[0], Intent::Move { .. }));
        assert_eq!(
            result[1],
            Intent::Break {
                position: BlockPos { x: -1, y: 3, z: 0 }
            }
        );
        input.player_block_actions = Some(vec![
            input.player_block_actions.unwrap()[0].clone();
            MAX_BLOCK_ACTIONS + 1
        ]);
        assert!(decode(&V2225::PlayerAuthInputPacket(Box::new(input)), 1).is_err());
    }

    #[test]
    fn explicit_item_use_place_and_destroy_are_requests_not_stack_replacements() {
        assert_eq!(
            decode(&roundtrip(&transaction(0)), 1).unwrap(),
            [Intent::Place {
                against: BlockPos { x: -1, y: 3, z: 0 },
                face: 1,
                hotbar_slot: 0,
                claimed_item_network_id: 1,
            }]
        );
        assert_eq!(
            decode(&roundtrip(&transaction(2)), 1).unwrap(),
            [Intent::Break {
                position: BlockPos { x: -1, y: 3, z: 0 },
            }]
        );
        for kind in [-1, 1, 3] {
            assert!(decode(&transaction(kind), 1).is_err());
        }
    }

    #[test]
    fn item_use_rejects_forged_slot_face_count_hand_nbt_size_and_coordinates() {
        for bad in 0..7 {
            let mut request = item_use_request(0);
            match bad {
                0 => request.hotbar_slot = 9,
                1 => request.block_face = 6,
                2 => request.held_item.stack_size = 65,
                3 => request.hand = 1,
                4 => request.held_item.user_data_buffer = vec![0; MAX_ITEM_USER_BYTES + 1],
                5 => request.click_position.0 = f32::NAN,
                _ => request.block_position.y = MAX_Y + 1,
            }
            assert!(item_use(&request).is_err());
        }
        let V2225::InventoryTransactionPacket(mut request) = transaction(0) else {
            unreachable!()
        };
        request.legacy_request_id = -2;
        assert!(decode(&V2225::InventoryTransactionPacket(request), 1).is_err());
    }

    #[test]
    fn unsupported_inventory_transactions_cannot_masquerade_as_noops() {
        let packet = V2225::InventoryTransactionPacket(Box::new(InventoryTransactionPacket {
            legacy_request_id: 0,
            legacy_set_item_slots: None,
            actions: vec![],
            data: InventoryTransactionData::Normal,
        }));
        assert!(
            decode(&roundtrip(&packet), 1)
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );
        let packet = V2225::ItemStackRequestPacket(Box::new(
            bedrock::protocol::v662::packets::ItemStackRequestPacket { requests: vec![] },
        ));
        assert!(decode(&roundtrip(&packet), 1).is_err());
    }

    #[test]
    fn slot_selection_validates_actor_container_and_slot_without_trusting_stack() {
        let equipment = MobEquipmentPacket {
            target_runtime_id: ActorRuntimeID(1),
            item: item(),
            slot: 2,
            selected_slot: 2,
            container_id: ContainerID::INVENTORY,
        };
        let packet = V2225::MobEquipmentPacket(Box::new(equipment.clone()));
        assert_eq!(
            decode(&roundtrip(&packet), 1).unwrap(),
            [Intent::SelectSlot { slot: 2 }]
        );
        assert!(decode(&packet, 2).is_err());
        let mut forged = equipment;
        forged.container_id = ContainerID::ARMOR;
        assert!(decode(&V2225::MobEquipmentPacket(Box::new(forged)), 1).is_err());
        let packet = V2225::PlayerHotbarPacket(Box::new(PlayerHotbarPacket {
            selected_slot: 8,
            container_id: ContainerID::INVENTORY,
            should_select_slot: true,
        }));
        assert_eq!(
            decode(&roundtrip(&packet), 1).unwrap(),
            [Intent::SelectSlot { slot: 8 }]
        );
    }

    #[test]
    fn keepalive_is_not_a_world_mutation() {
        let packet = V2225::NetworkStackLatencyPacket(Box::new(NetworkStackLatencyPacket {
            creation_time: 4,
            is_from_server: false,
        }));
        assert!(decode(&roundtrip(&packet), 1).unwrap().is_empty());
    }
}
