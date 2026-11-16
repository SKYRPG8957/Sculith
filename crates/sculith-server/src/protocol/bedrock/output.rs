//! Edition encoding of authoritative decisions. Numeric item IDs are V2225 facts
//! from Dragonfly's vanilla_items.nbt at 0f9a3a5736267db60bf46e92d408f81be1de9a64.
use super::inventory::stack;
use crate::game::PlayerSnapshot;
use anyhow::{Result, bail};
use bedrock::protocol::{
    V2225,
    v662::{
        enums::CommandPermissionLevel,
        packets::{UpdateAbilitiesPacket, UpdateBlockPacket},
        types::{ActorRuntimeID, ActorUniqueID},
    },
    v729::types::FullContainerName,
    v776::{
        enums::ItemVersion,
        packets::{ItemComponentPacket, ItemsEntry},
        types::{SerializedAbilitiesData, SerializedAbilitiesLayer, SerializedLayer},
    },
    v944::types::NetworkBlockPosition,
    v975::packets::MobEquipmentPacket,
    v1001::packets::InventoryContentPacket,
    v2168::{
        enums::PlayerPositionMode,
        packets::MovePlayerPacket,
        types::{MovePlayerTeleportData, NetworkItemStackDescriptorV2},
    },
    v2225::enums::ContainerEnumName,
};
use sculith_world::{BlockKind, BlockPos};

pub fn chat(message: &crate::game::ChatMessage) -> V2225 {
    V2225::TextPacket(Box::new(bedrock::protocol::v898::packets::TextPacket {
        localize: false,
        message_type: bedrock::protocol::v924::enums::TextPacketType::Chat {
            player_name: message.name.clone(),
            message: message.message.clone(),
        },
        sender_xuid: String::new(),
        platform_id: String::new(),
        filtered_message: None,
    }))
}

pub fn system_message(message: &str) -> Result<V2225> {
    sculith_core::validate_system_message(message).map_err(anyhow::Error::msg)?;
    Ok(V2225::TextPacket(Box::new(
        bedrock::protocol::v898::packets::TextPacket {
            localize: false,
            message_type: bedrock::protocol::v924::enums::TextPacketType::SystemMessage(
                message.into(),
            ),
            sender_xuid: String::new(),
            platform_id: String::new(),
            filtered_message: None,
        },
    )))
}

pub fn block_update(position: BlockPos, block: BlockKind) -> Result<V2225> {
    Ok(V2225::UpdateBlockPacket(Box::new(UpdateBlockPacket {
        block_position: NetworkBlockPosition {
            x: position.x,
            y: position.y,
            z: position.z,
        },
        block_runtime_id: super::chunk::block_hash(block)?,
        flags: 3,
        layer: 0,
    })))
}
pub fn movement(player: &PlayerSnapshot, tick: u64, correction: bool) -> V2225 {
    V2225::MovePlayerPacket(Box::new(MovePlayerPacket {
        player_runtime_id: ActorRuntimeID(player.id),
        position: (
            player.feet.x as f32,
            player.feet.y as f32 + super::spawn::EYE_OFFSET,
            player.feet.z as f32,
        ),
        rotation: (player.rotation.pitch as f32, player.rotation.yaw as f32),
        y_head_rotation: player.rotation.head_yaw as f32,
        position_mode: if correction {
            PlayerPositionMode::Teleport
        } else {
            PlayerPositionMode::Normal
        },
        on_ground: false,
        riding_runtime_id: ActorRuntimeID(0),
        teleport_data: correction.then_some(MovePlayerTeleportData {
            teleportation_cause: 0,
            source_actor_type: 0,
        }),
        tick,
    }))
}
pub fn claimed_block(id: i32) -> Result<BlockKind> {
    Ok(match id {
        1 => BlockKind::Stone,
        3 => BlockKind::Dirt,
        2 => BlockKind::Grass,
        7 => BlockKind::Bedrock,
        _ => bail!("item is not in the server starter registry"),
    })
}
pub fn inventory(player: &PlayerSnapshot) -> Result<Vec<V2225>> {
    let mut slots = Vec::with_capacity(36);
    for item in player.inventory {
        slots.push(stack(item)?);
    }
    Ok(vec![
        V2225::InventoryContentPacket(Box::new(InventoryContentPacket {
            inventory_id: 0,
            slots,
            container_name_data: FullContainerName {
                container: ContainerEnumName::InventoryContainer,
                dynamic_id: None,
            },
            storage_item: stack(None)?,
        })),
        equipment(player)?,
    ])
}
pub fn held(player: &PlayerSnapshot) -> Result<NetworkItemStackDescriptorV2> {
    anyhow::ensure!(player.selected_slot <= 8, "invalid owner hotbar slot");
    stack(player.held())
}
pub fn inventory_ui(player: &PlayerSnapshot) -> Result<Vec<V2225>> {
    use bedrock::protocol::v975::packets::InventorySlotPacket;
    [
        (0, ContainerEnumName::CursorContainer, player.cursor),
        (
            50,
            ContainerEnumName::CreatedOutputContainer,
            player.created_output,
        ),
    ]
    .into_iter()
    .map(|(slot, container, item)| {
        Ok(V2225::InventorySlotPacket(Box::new(InventorySlotPacket {
            container_id: 124,
            slot,
            container_name_data: Some(FullContainerName {
                container,
                dynamic_id: None,
            }),
            storage_item: None,
            item: stack(item)?,
        })))
    })
    .collect()
}
pub fn equipment(player: &PlayerSnapshot) -> Result<V2225> {
    Ok(V2225::MobEquipmentPacket(Box::new(MobEquipmentPacket {
        target_runtime_id: ActorRuntimeID(player.id),
        item: held(player)?,
        slot: player.selected_slot as i8,
        selected_slot: player.selected_slot as i8,
        container_id: bedrock::protocol::v662::enums::ContainerID::INVENTORY,
    })))
}
pub fn registry_and_abilities(player: &PlayerSnapshot) -> Vec<V2225> {
    let items = [
        ("stone", 1),
        ("dirt", 3),
        ("grass_block", 2),
        ("bedrock", 7),
    ]
    .into_iter()
    .map(|(name, id)| ItemsEntry {
        component_item_name: format!("minecraft:{name}"),
        runtime_id: id,
        is_component_based: false,
        version: ItemVersion::None,
        component_data: nbtx::Value::Compound(Default::default()),
    })
    .collect();
    vec![
        V2225::ItemComponentPacket(Box::new(ItemComponentPacket { items })),
        V2225::UpdateAbilitiesPacket(Box::new(UpdateAbilitiesPacket {
            data: abilities(player),
        })),
    ]
}
/// The same authoritative capabilities accompany local and peer presentations.
pub fn abilities(player: &PlayerSnapshot) -> SerializedAbilitiesData<V2225> {
    SerializedAbilitiesData {
        target_player_raw_id: player.id as i64,
        player_permissions: 1,
        command_permissions: CommandPermissionLevel::Any,
        layers: vec![SerializedLayer {
            serialized_layer: SerializedAbilitiesLayer::Base,
            // gophertunnel protocol/ability.go at e2251a60: explicitly
            // define all current flags, without operator/teleport/noclip.
            abilities_set: (1 << 20) - 1,
            ability_values: if player.permissions.can_build {
                3 | (1 << 8) | (1 << 10) | (1 << 11) | (1 << 13) | (1 << 14) | (1 << 19)
            } else {
                0
            },
            fly_speed: 0.05,
            vertical_fly_speed: 1.0,
            walk_speed: 0.1,
        }],
    }
}

pub fn simple_form(id: u32, form: &sculith_core::SimpleForm) -> anyhow::Result<V2225> {
    form.validate().map_err(anyhow::Error::msg)?;
    let buttons: Vec<_> = form
        .buttons
        .iter()
        .map(|text| serde_json::json!({"text":text}))
        .collect();
    Ok(V2225::ModalFormRequestPacket(Box::new(
        bedrock::protocol::v662::packets::ModalFormRequestPacket {
            form_id: id,
            form_ui_json: serde_json::to_string(
                &serde_json::json!({"type":"form","title":form.title,"content":form.body,"buttons":buttons}),
            )?,
        },
    )))
}

/// Same fixed-point position and parameters as the Java direct sound event.
pub fn sound(effect: &crate::game::SoundEffect, native: &str) -> V2225 {
    use bedrock::protocol::v2193::packets::{PlaySoundPacket, PlaySoundPosition};
    let p = effect.position;
    V2225::PlaySoundPacket(Box::new(PlaySoundPacket {
        name: native.into(),
        position: PlaySoundPosition::from_raw_x8(
            (p.x * 8.0) as i32,
            (p.y * 8.0) as i32,
            (p.z * 8.0) as i32,
        ),
        volume: effect.volume,
        pitch: effect.pitch,
        loop_count: -1,
        bypass_listener_range_check: false,
        server_sound_handle: None,
        playback_position_seconds: None,
    }))
}

/// Only resource-backed presentation actors are registered by this preview.
pub fn model_registry(models: &[sculith_packs::ModelMapping]) -> V2225 {
    use nbtx::{Value, ValueList};
    let values = models
        .iter()
        .map(|entry| {
            [("id".into(), Value::String(entry.bedrock.as_bytes().into()))]
                .into_iter()
                .collect()
        })
        .collect();
    V2225::AvailableActorIdentifiersPacket(Box::new(
        bedrock::protocol::v662::packets::AvailableActorIdentifiersPacket {
            actor_info_list: [("idlist".into(), Value::List(ValueList::Compound(values)))]
                .into_iter()
                .collect(),
        },
    ))
}

pub fn remove_model(entity_id: u64) -> V2225 {
    V2225::RemoveActorPacket(Box::new(
        bedrock::protocol::v662::packets::RemoveActorPacket {
            target_actor_id: ActorUniqueID(entity_id as i64),
        },
    ))
}

pub fn show_model(view: &crate::game::ModelView, native: &str) -> Vec<V2225> {
    vec![
        remove_model(view.entity_id),
        V2225::AddActorPacket(Box::new(
            bedrock::protocol::v2225::packets::AddActorPacket {
                target_actor_id: ActorUniqueID(view.entity_id as i64),
                target_runtime_id: ActorRuntimeID(view.entity_id),
                actor_type: native.into(),
                position: (
                    view.position.x as f32,
                    view.position.y as f32,
                    view.position.z as f32,
                ),
                velocity: (0.0, 0.0, 0.0),
                rotation: (0.0, 0.0),
                y_head_rotation: 0.0,
                y_body_rotation: 0.0,
                attributes: vec![],
                actor_data: vec![],
                synced_properties: bedrock::protocol::v662::types::PropertySyncData {
                    int_entries_list: vec![],
                    float_entries_list: vec![],
                },
                actor_links: vec![],
                passenger_of_block_data: None,
            },
        )),
    ]
}
