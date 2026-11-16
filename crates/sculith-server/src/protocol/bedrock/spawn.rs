//! Flat-world initialization and bounded chunk interest for the builder preview.

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
pub type Interest = std::collections::BTreeMap<ChunkPos, u64>;

pub fn initial_packets(player: &crate::game::PlayerSnapshot) -> Result<Vec<V2225>> {
    let mut packets = vec![
        // V2225 clients fail with MissingStructureData (148) if StartGame
        // precedes this registry, even when all four structure lists are empty.
        structure_data(),
        V2225::VoxelShapesPacket(Box::new(VoxelShapesPacket {
            shapes: vec![],
            names: vec![],
            custom_shape_count: 0,
        })),
        V2225::StartGamePacket(Box::new(start_game(player))),
        V2225::UpdateAdventureSettingsPacket(Box::new(UpdateAdventureSettingsPacket {
            adventure_settings: AdventureSettings {
                no_pvm: true,
                no_mvp: true,
                immutable_world: !player.permissions.can_build,
                show_name_tags: true,
                auto_jump: false,
            },
        })),
    ];
    packets.extend(super::output::registry_and_abilities(player));
    packets.extend(super::output::inventory(player)?);
    packets.extend(super::output::inventory_baseline(player)?);
    // Chunk biome palettes reference the native plains biome. Supply its
    // definition before chunks, even though headless clients do not render it.
    packets.push(plains_definition());
    packets.push(super::inventory::creative_catalogue()?);
    Ok(packets)
}

fn structure_data() -> V2225 {
    use nbtx::{Value, ValueList};
    let registry = ["processors", "template_pools", "jigsaws", "structure_sets"]
        .into_iter()
        .map(|name| (name.into(), Value::List(ValueList::Compound(vec![]))))
        .collect();
    V2225::JigsawStructureDataPacket(Box::new(
        bedrock::protocol::v712::packets::JigsawStructureDataPacket {
            jigsaw_structure_data_tag: Value::Compound(registry),
        },
    ))
}

/// V2225 definition layout from bedrock-rs 5221fcae; native plains parameters
/// and the -1 built-in-ID sentinel checked against Dragonfly 0f9a3a57,
/// server/world/biome{_definition.go,/plains.go}. This preview generates its
/// own terrain; no client-side biome generation data or assets are needed.
fn plains_definition() -> V2225 {
    use bedrock::protocol::{
        v800::packets::{BiomeDefinitionListPacket, BiomeEntry},
        v844::types::{BiomeDefinition, BiomeTagList},
    };
    V2225::BiomeDefinitionListPacket(Box::new(BiomeDefinitionListPacket {
        biomes: vec![BiomeEntry {
            name_index: 0,
            definition: BiomeDefinition {
                id: u16::MAX,
                temperature: 0.8,
                downfall: 0.4,
                foliage_snow: 0.0,
                depth: 0.125,
                scale: 0.05,
                map_water_color: 0xa544aff5u32 as i32,
                rain: true,
                tags: Some(BiomeTagList {
                    tags: vec![1, 2, 3, 0, 4],
                }),
                chunk_gen_data: None,
            },
        }],
        strings: ["plains", "animal", "monster", "overworld", "bee_habitat"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    }))
}

fn start_game(player: &crate::game::PlayerSnapshot) -> StartGamePacket<V2225> {
    StartGamePacket {
        target_actor_id: ActorUniqueID(player.id as i64),
        target_runtime_id: ActorRuntimeID(player.id),
        actor_game_type: GameType::Creative,
        position: (
            player.feet.x as f32,
            player.feet.y as f32 + EYE_OFFSET,
            player.feet.z as f32,
        ),
        rotation: (0.0, 0.0),
        settings: LevelSettings {
            seed: 0,
            spawn_settings: SpawnSettings {
                spawn_type: SpawnBiomeType::Default,
                user_defined_biome_name: String::new(),
                dimension: 0,
            },
            generator_type: GeneratorType::Overworld,
            game_type: GameType::Creative,
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
            disable_player_interactions: false,
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
        enable_item_stack_net_manager: true,
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
) -> Result<Interest> {
    let mut interest = Interest::new();
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
            interest.insert(snapshot.position(), snapshot.revision());
            conn.send(&[V2225::LevelChunkPacket(Box::new(packet))])
                .await?;
        }
    }
    conn.send(&[V2225::PlayStatusPacket(Box::new(PlayStatusPacket {
        status: PlayStatus::PlayerSpawn,
    }))])
    .await?;
    Ok(interest)
}

pub async fn refresh<T: Transport>(
    conn: &mut Connection<V2225, T>,
    world: &WorldHandle,
    interest: &mut Interest,
    player: &crate::game::PlayerSnapshot,
    radius: i32,
) -> Result<()> {
    let center = ChunkPos {
        x: (player.feet.x.floor() as i32).div_euclid(16),
        z: (player.feet.z.floor() as i32).div_euclid(16),
    };
    let bounds = world.capture().await?.bounds();
    interest.retain(|position, _| {
        (position.x - center.x).abs() <= radius && (position.z - center.z).abs() <= radius
    });
    conn.send(&[V2225::NetworkChunkPublisherUpdatePacket(Box::new(
        NetworkChunkPublisherUpdatePacket {
            new_view_position: BlockPos {
                x: player.feet.x.floor() as i32,
                y: player.feet.y.floor() as i32,
                z: player.feet.z.floor() as i32,
            },
            new_view_radius: (radius * 16) as u32,
            server_built_chunks: vec![],
        },
    ))])
    .await?;
    for z in center.z - radius..=center.z + radius {
        for x in center.x - radius..=center.x + radius {
            let position = ChunkPos { x, z };
            if bounds.contains(position) && !interest.contains_key(&position) {
                let snapshot = world.snapshot(position).await?;
                conn.send(&[V2225::LevelChunkPacket(Box::new(
                    super::chunk::encode_chunk(&snapshot)?,
                ))])
                .await?;
                interest.insert(position, snapshot.revision());
            }
        }
    }
    Ok(())
}
