//! Native player presentation with an original solid-colour placeholder skin.
//! Account skins/capes/persona transfer are deliberately outside this preview.
use crate::game::PlayerSnapshot;
use anyhow::Result;
use bedrock::{
    network::tokio::{Connection, transport::Transport},
    protocol::{
        ProtoCodec, V2225,
        v662::{
            enums::{BuildPlatform, GameType},
            types::{ActorRuntimeID, ActorUniqueID, PropertySyncData},
        },
        v2168::{enums::ArmSizeType, types::NetworkItemStackDescriptor},
        v2225::{
            packets::{AddPlayerListEntry, AddPlayerPacket, PlayerListEntry, PlayerListPacket},
            types::SerializedSkin,
        },
    },
};
use sculith_server::peers::Change;

pub async fn send<T: Transport>(
    conn: &mut Connection<V2225, T>,
    changes: Vec<Change>,
) -> Result<()> {
    for change in changes {
        let packet = match change {
            Change::InfoAdd(player) => info(&player)?,
            Change::InfoRemove { uuid } => V2225::PlayerListPacket(Box::new(PlayerListPacket {
                entries: vec![PlayerListEntry::Remove { uuid }],
            })),
            Change::Spawn(player) => spawn(&player)?,
            Change::Equipment(player) => super::output::equipment(&player)?,
            Change::Move { player, tick } => super::output::movement(&player, tick, false),
            Change::Despawn { id } => super::output::remove_model(id),
        };
        // One bounded player entry per batch, retaining the negotiated
        // compression/encryption and transport queue semantics.
        conn.send(&[packet]).await?;
    }
    Ok(())
}

fn info(player: &PlayerSnapshot) -> Result<V2225> {
    let color = bedrock::protocol::v800::types::Color::deserialize(&mut &[255u8; 4][..])?;
    Ok(V2225::PlayerListPacket(Box::new(PlayerListPacket {
        entries: vec![PlayerListEntry::Add(AddPlayerListEntry {
            uuid: player.profile.uuid,
            target_actor_id: ActorUniqueID(player.id as i64),
            player_name: player.profile.name.clone(),
            xbl_xuid: String::new(),
            play_fab_id: String::new(),
            platform_chat_id: String::new(),
            build_platform: BuildPlatform::Dedicated,
            serialized_skin: skin(player),
            is_teacher: false,
            is_host: false,
            is_sub_client: false,
            color,
        })],
    })))
}

fn skin(player: &PlayerSnapshot) -> SerializedSkin<V2225> {
    SerializedSkin {
        skin_id: "sculith-preview-player-v1".into(),
        skin_resource_patch: r#"{"geometry":{"default":"geometry.sculith.preview_player"}}"#.into(),
        skin_image_width: 64,
        skin_image_height: 64,
        skin_image_bytes: [75, 145, 85, 255].repeat(64 * 64),
        animations: vec![],
        cape_image_width: 0,
        cape_image_height: 0,
        cape_image_bytes: vec![],
        geometry_data: include_str!("preview_player_geometry.json").into(),
        geometry_data_engine_version: "1.26.0".into(),
        animation_data: String::new(),
        cape_id: String::new(),
        full_id: format!("sculith-preview-{}", player.profile.uuid),
        arm_size: ArmSizeType::Wide,
        skin_color: -1,
        persona_pieces: vec![],
        piece_tint_colors: vec![],
        is_premium_skin: false,
        is_persona_skin: false,
        is_persona_cape_on_classic_skin: false,
        is_primary_user: true,
        overrides_player_appearance: true,
        trusted_skin_flag: "True".into(),
        profile_hash: String::new(),
    }
}

fn spawn(player: &PlayerSnapshot) -> Result<V2225> {
    let held = super::output::held(player)?;
    Ok(V2225::AddPlayerPacket(Box::new(AddPlayerPacket {
        uuid: player.profile.uuid,
        player_name: player.profile.name.clone(),
        target_runtime_id: ActorRuntimeID(player.id),
        platform_chat_id: String::new(),
        position: (
            player.feet.x as f32,
            player.feet.y as f32 + super::spawn::EYE_OFFSET,
            player.feet.z as f32,
        ),
        velocity: (0.0, 0.0, 0.0),
        rotation: (player.rotation.pitch as f32, player.rotation.yaw as f32),
        y_head_rotation: player.rotation.head_yaw as f32,
        carried_item: NetworkItemStackDescriptor {
            id: held.id,
            stack_size: held.stack_size,
            aux_value: held.aux_value,
            net_id: held.net_id,
            block_runtime_id: held.block_runtime_id,
            user_data_buffer: held.user_data_buffer,
        },
        player_game_type: GameType::Creative,
        entity_data: vec![],
        synced_properties: PropertySyncData {
            int_entries_list: vec![],
            float_entries_list: vec![],
        },
        abilities_data: super::output::abilities(player),
        actor_links: vec![],
        passenger_of_block_data: None,
        device_id: String::new(),
        build_platform: BuildPlatform::Dedicated,
    })))
}
