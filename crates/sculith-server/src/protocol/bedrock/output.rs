//! Edition encoding of authoritative decisions. Numeric item IDs are V2225 facts
//! from Dragonfly's vanilla_items.nbt at 0f9a3a5736267db60bf46e92d408f81be1de9a64.
use crate::game::PlayerSnapshot;
use anyhow::{Result, bail};
use bedrock::protocol::{
    V2225,
    v662::{
        enums::CommandPermissionLevel,
        packets::{UpdateAbilitiesPacket, UpdateBlockPacket},
        types::ActorRuntimeID,
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
        _ => bail!("item is not in the server starter registry"),
    })
}
fn item_id(kind: BlockKind) -> Result<i16> {
    Ok(match kind {
        BlockKind::Stone => 1,
        BlockKind::Dirt => 3,
        BlockKind::Grass => 2,
        BlockKind::Bedrock => 7,
        BlockKind::Air => 0,
    })
}
fn stack(kind: Option<BlockKind>) -> Result<NetworkItemStackDescriptorV2> {
    Ok(NetworkItemStackDescriptorV2 {
        id: kind.map(item_id).transpose()?.unwrap_or(0),
        stack_size: if kind.is_some() { 64 } else { 0 },
        aux_value: 0,
        net_id: None,
        block_runtime_id: kind.map(super::chunk::block_hash).transpose()?.unwrap_or(0),
        user_data_buffer: if kind.is_some() { vec![0; 10] } else { vec![] },
    })
}
pub fn inventory(player: &PlayerSnapshot) -> Result<Vec<V2225>> {
    let mut slots = Vec::with_capacity(36);
    for index in 0..36 {
        slots.push(stack(player.hotbar.get(index).copied().flatten())?);
    }
    let selected = stack(player.hotbar[player.selected_slot as usize])?;
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
        V2225::MobEquipmentPacket(Box::new(MobEquipmentPacket {
            target_runtime_id: ActorRuntimeID(player.id),
            item: selected,
            slot: player.selected_slot as i8,
            selected_slot: player.selected_slot as i8,
            container_id: bedrock::protocol::v662::enums::ContainerID::INVENTORY,
        })),
    ])
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
            data: SerializedAbilitiesData {
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
            },
        })),
    ]
}
