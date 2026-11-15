//! Read-only flat-world spawn. Block interaction and movement commits follow separately.

use anyhow::Result;
use bedrock::{
    network::tokio::{Connection, transport::Transport},
    protocol::{
        ProtoVersion, V2225,
        v662::{
            enums::{
                ChatRestrictionLevel, Difficulty, EditorWorldType, GamePublishSetting, GameType,
                GeneratorType, PlayStatus, SpawnBiomeType,
            },
            packets::{
                ChunkRadiusUpdatedPacket, NetworkChunkPublisherUpdatePacket, PlayStatusPacket,
                UpdateAdventureSettingsPacket,
            },
            types::{
                ActorRuntimeID, ActorUniqueID, AdventureSettings, BaseGameVersion, BlockPos,
                EduSharedUriResource, Experiments, NetworkPermissions, SpawnSettings,
            },
        },
        v818::types::SyncedPlayerMovementSettings,
        v944::{packets::VoxelShapesPacket, types::NetworkBlockPosition},
        v2168::{enums::EducationEditionOffer, packets::StartGamePacket},
        v2225::types::{EditorLevelMigrationVersion, GameRuleLegacyData, LevelSettings},
    },
};
use sculith_world::ChunkPos;

use crate::world::WorldHandle;

pub const RADIUS: i32 = 2;
/// Bedrock wire positions use the player's base/eye offset above world feet.
pub const EYE_OFFSET: f32 = 1.62;

pub fn initial_packets(entity_id: u64) -> Vec<V2225> {
    vec![
        V2225::VoxelShapesPacket(Box::new(VoxelShapesPacket {
            shapes: vec![],
            names: vec![],
            custom_shape_count: 0,
        })),
        V2225::StartGamePacket(Box::new(start_game(entity_id))),
        V2225::UpdateAdventureSettingsPacket(Box::new(UpdateAdventureSettingsPacket {
            adventure_settings: AdventureSettings {
                no_pvm: true,
                no_mvp: true,
                immutable_world: true,
                show_name_tags: true,
                auto_jump: false,
            },
        })),
    ]
}

fn start_game(entity_id: u64) -> StartGamePacket<V2225> {
    StartGamePacket {
        target_actor_id: ActorUniqueID(entity_id as i64),
        target_runtime_id: ActorRuntimeID(entity_id),
        actor_game_type: GameType::Adventure,
        position: (0.5, 4.0 + EYE_OFFSET, 0.5),
        rotation: (0.0, 0.0),
        settings: LevelSettings {
            seed: 0,
            spawn_settings: SpawnSettings {
                spawn_type: SpawnBiomeType::Default,
                user_defined_biome_name: String::new(),
                dimension: 0,
            },
            generator_type: GeneratorType::Overworld,
            game_type: GameType::Adventure,
            is_hardcore_enabled: false,
            game_difficulty: Difficulty::Peaceful,
            default_spawn_block_position: NetworkBlockPosition { x: 0, y: 4, z: 0 },
            achievements_disabled: true,
            editor_world_type: EditorWorldType::NonEditor,
            is_created_in_editor: false,
            is_exported_from_editor: false,
            editor_level_migration_version: EditorLevelMigrationVersion::Legacy,
            day_cycle_stop_time: 6000,
            education_edition_offer: EducationEditionOffer::None,
            education_features_enabled: false,
            education_product_id: String::new(),
            rain_level: 0.0,
            lightning_level: 0.0,
            has_confirmed_platform_locked_content: false,
            multiplayer_enabled: true,
            lan_broadcasting_enabled: false,
            xbox_live_broadcast_setting: GamePublishSetting::FriendsOnly,
            platform_broadcast_setting: GamePublishSetting::FriendsOnly,
            commands_enabled: false,
            texture_packs_required: false,
            rule_data: GameRuleLegacyData { rules_list: vec![] },
            experiments: Experiments {
                experiments: vec![],
                ever_toggled: false,
            },
            bonus_chest_enabled: false,
            starting_map_enabled: false,
            player_permissions: 1,
            server_chunk_tick_range: 0,
            locked_behaviour_pack: false,
            locked_resource_pack: false,
            from_locked_template: false,
            use_msa_gamer_tags: false,
            from_template: false,
            has_locked_template_settings: false,
            only_spawn_v1_villagers: false,
            persona_disabled: false,
            custom_skins_disabled: false,
            emote_chat_muted: false,
            base_game_version: BaseGameVersion(V2225::GAME_VERSION.into()),
            limited_world_width: 0,
            limited_world_depth: 0,
            nether_type: true,
            edu_shared_uri_resource: EduSharedUriResource {
                button_name: String::new(),
                link_uri: String::new(),
            },
            override_force_experimental_gameplay: None,
            chat_restriction_level: ChatRestrictionLevel::None,
            disable_player_interactions: true,
            server_editor_connection_policy: 0,
            allow_anonymous_block_drops_in_editor_worlds: false,
        },
        level_id: "sculith-flat-preview".into(),
        level_name: "Sculith flat preview".into(),
        template_content_identity: String::new(),
        is_trial: false,
        movement_settings: SyncedPlayerMovementSettings {
            rewind_history_size: 0,
            server_authoritative_block_breaking: true,
        },
        current_level_time: 6000,
        enchantment_seed: 0,
        block_properties: vec![],
        multiplayer_correlation_id: String::new(),
        enable_item_stack_net_manager: false,
        server_version: format!("Sculith {}", env!("CARGO_PKG_VERSION")),
        player_property_data: Default::default(),
        server_block_type_registry_checksum: 0,
        world_template_id: uuid::Uuid::nil(),
        server_enabled_client_side_generation: false,
        block_network_ids_are_hashes: true,
        network_permissions: NetworkPermissions {
            server_auth_sound_enabled: false,
        },
        server_join_information: None,
        server_id: String::new(),
        world_id: String::new(),
        scenario_id: String::new(),
        owner_id: String::new(),
    }
}

pub async fn send_chunks<T: Transport>(
    conn: &mut Connection<V2225, T>,
    world: &WorldHandle,
    radius: i32,
) -> Result<()> {
    conn.send(&[
        V2225::ChunkRadiusUpdatedPacket(Box::new(ChunkRadiusUpdatedPacket {
            chunk_radius: radius,
        })),
        V2225::NetworkChunkPublisherUpdatePacket(Box::new(NetworkChunkPublisherUpdatePacket {
            new_view_position: BlockPos { x: 0, y: 4, z: 0 },
            new_view_radius: (radius * 16) as u32,
            server_built_chunks: vec![],
        })),
    ])
    .await?;
    for z in -radius..=radius {
        for x in -radius..=radius {
            let snapshot = world.snapshot(ChunkPos { x, z }).await?;
            let packet = super::chunk::encode_chunk(&snapshot)?;
            conn.send(&[V2225::LevelChunkPacket(Box::new(packet))])
                .await?;
        }
    }
    conn.send(&[V2225::PlayStatusPacket(Box::new(PlayStatusPacket {
        status: PlayStatus::PlayerSpawn,
    }))])
    .await?;
    Ok(())
}
