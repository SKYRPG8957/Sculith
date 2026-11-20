use pumpkin_data::biome::Biome;

use crate::world_info::WorldInfoReader;

use super::*;

const FLAT_LAYERS: &str = r#"{"biome_id":1,"block_layers":[{"block_name":"minecraft:stone","count":3}],"encoding_version":6,"structure_options":null,"world_version":"version.post_1_18"}"#;

fn named_tag(nbt: &mut Vec<u8>, type_id: u8, key: &str, bytes: &[u8]) {
    nbt.push(type_id);
    nbt.extend_from_slice(&u16::try_from(key.len()).unwrap().to_le_bytes());
    nbt.extend_from_slice(key.as_bytes());
    nbt.extend_from_slice(bytes);
}

fn block_entry(nbt: &mut Vec<u8>, name: &str, states: &[u8]) {
    nbt.extend_from_slice(&[pumpkin_nbt::COMPOUND_ID, 0, 0]);
    let mut string = u16::try_from(name.len()).unwrap().to_le_bytes().to_vec();
    string.extend_from_slice(name.as_bytes());
    named_tag(nbt, pumpkin_nbt::STRING_ID, "name", &string);
    named_tag(nbt, pumpkin_nbt::COMPOUND_ID, "states", states);
    nbt.push(0);
}

fn write_world(source: &Path, corrupt: bool, flat_layers: &str) {
    fs::create_dir(source).unwrap();
    let mut nbt = vec![pumpkin_nbt::COMPOUND_ID, 0, 0];
    named_tag(
        &mut nbt,
        pumpkin_nbt::LONG_ID,
        "RandomSeed",
        &(-764i64).to_le_bytes(),
    );
    for (name, value) in [
        ("Generator", 2i32),
        ("SpawnX", -3),
        ("SpawnY", -59),
        ("SpawnZ", 3),
    ] {
        named_tag(&mut nbt, pumpkin_nbt::INT_ID, name, &value.to_le_bytes());
    }
    string_tag(&mut nbt, "FlatWorldLayers", flat_layers);
    nbt.push(0);
    let mut level_dat = STORAGE_VERSION.to_le_bytes().to_vec();
    level_dat.extend_from_slice(&u32::try_from(nbt.len()).unwrap().to_le_bytes());
    level_dat.extend_from_slice(&nbt);
    fs::write(source.join("level.dat"), level_dat).unwrap();

    let db = DB::open(source.join("db"), Options::default()).unwrap();
    let mut key = (-1i32).to_le_bytes().to_vec();
    key.extend_from_slice(&0i32.to_le_bytes());
    key.push(VERSION);
    db.put(&key, &[CHUNK_VERSION]).unwrap();
    key[8] = BIOMES;
    let mut biomes = vec![0; 512];
    biomes.push(1); // Official disk biome palettes can set the low bit.
    biomes.extend_from_slice(&u32::from(Biome::PLAINS.be_network_id).to_le_bytes());
    biomes.extend(std::iter::repeat_n(0xff, section_count() - 1));
    db.put(&key, &biomes).unwrap();
    key[8] = SUBCHUNK;
    key.push((-4i8).cast_unsigned());
    let mut section = vec![9, 1, (-4i8).cast_unsigned(), 4]; // Two bits per block.
    let mut indices = [0u32; 256];
    let marker = (13 * 16 + 3) * 16 + 4; // Bedrock's X/Z/Y ordering.
    indices[marker / 16] |= 1 << ((marker % 16) * 2);
    let liquid = marker + 1;
    indices[liquid / 16] |= 2 << ((liquid % 16) * 2);
    for word in indices {
        section.extend_from_slice(&word.to_le_bytes());
    }
    section.extend_from_slice(&3u32.to_le_bytes());
    block_entry(&mut section, "minecraft:air", &[0]);
    block_entry(
        &mut section,
        if corrupt {
            "example:unmapped"
        } else {
            "minecraft:diamond_block"
        },
        &[0],
    );
    let mut fluid_state = Vec::new();
    named_tag(
        &mut fluid_state,
        pumpkin_nbt::INT_ID,
        "liquid_depth",
        &[0; 4],
    );
    fluid_state.push(0);
    block_entry(&mut section, "minecraft:flowing_water", &fluid_state);
    db.put(&key, &section).unwrap();
    key[9] = (-3i8).cast_unsigned();
    db.put(&key, &[9, 0, key[9]]).unwrap();
    db.close().unwrap();
}

fn snapshot(source: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    std::iter::once(source.join("level.dat"))
        .chain(
            fs::read_dir(source.join("db"))
                .unwrap()
                .map(|entry| entry.unwrap().path()),
        )
        .map(|path| (path.clone(), fs::read(path).unwrap()))
        .collect()
}

fn string_tag(nbt: &mut Vec<u8>, key: &str, value: &str) {
    let mut bytes = u16::try_from(value.len()).unwrap().to_le_bytes().to_vec();
    bytes.extend_from_slice(value.as_bytes());
    named_tag(nbt, pumpkin_nbt::STRING_ID, key, &bytes);
}

fn container(nbt: &mut Vec<u8>, kind: &str, x: i32, items: &[(u8, &str, u8)]) {
    nbt.extend_from_slice(&[pumpkin_nbt::COMPOUND_ID, 0, 0]);
    string_tag(nbt, "id", kind);
    for (name, value) in [("x", x), ("y", -60), ("z", 0)] {
        named_tag(nbt, pumpkin_nbt::INT_ID, name, &value.to_le_bytes());
    }
    let mut list = vec![pumpkin_nbt::COMPOUND_ID];
    list.extend_from_slice(&i32::try_from(items.len()).unwrap().to_le_bytes());
    for &(slot, item, count) in items {
        string_tag(&mut list, "Name", item);
        named_tag(&mut list, pumpkin_nbt::BYTE_ID, "Slot", &[slot]);
        named_tag(&mut list, pumpkin_nbt::BYTE_ID, "Count", &[count]);
        list.push(0);
    }
    named_tag(nbt, pumpkin_nbt::LIST_ID, "Items", &list);
    nbt.push(0);
}

#[test]
fn preserves_valid_slots_without_importing_partial_containers() {
    let mut chunk = ChunkData::empty(-1, 0);
    chunk
        .section
        .set_block_absolute_y(15, -60, 0, Block::CHEST.default_state.id);
    chunk
        .section
        .set_block_absolute_y(14, -60, 0, Block::BARREL.default_state.id);
    chunk
        .section
        .set_block_absolute_y(13, -60, 0, Block::CHEST.default_state.id);
    let mut record = Vec::new();
    container(
        &mut record,
        "Chest",
        -1,
        &[
            (0, "minecraft:iron_ingot", 19),
            (26, "minecraft:diamond", 1),
        ],
    );
    container(
        &mut record,
        "Barrel",
        -2,
        &[(8, "minecraft:apple", 5), (8, "minecraft:apple", 2)],
    );
    container(
        &mut record,
        "Chest",
        -3,
        &[(0, "minecraft:apple", 3), (1, "minecraft:potion", 1)],
    );
    let mut stats = ImportStats::default();
    assert!(
        !storage::import_record(
            &record,
            &mut chunk,
            &terrain::TerrainRegistry::load().unwrap(),
            &storage::item_registry(),
            &mut stats
        )
        .unwrap()
    );
    assert_eq!(
        (stats.containers_imported, stats.block_entities_skipped),
        (1, 2)
    );
    let skipped = &stats.skipped_block_entity_examples[0];
    assert_eq!(skipped.position, Some(BlockPos::new(-2, -60, 0)));
    assert_eq!(skipped.reason, "duplicate slot 8");
    let potion = &stats.skipped_block_entity_examples[1];
    assert_eq!(potion.position, Some(BlockPos::new(-3, -60, 0)));
    assert!(potion.reason.contains("requires potion data conversion"));
    let entities = chunk.pending_block_entities.get_mut().unwrap();
    assert_eq!(entities.len(), 1);
    let chest = &entities[&BlockPos::new(-1, -60, 0)];
    assert_eq!(chest.get_string("id"), Some("minecraft:chest"));
    let actual: Vec<_> = chest
        .get_list("Items")
        .unwrap()
        .iter()
        .map(|item| {
            let item = item.extract_compound().unwrap();
            (
                item.get_byte("Slot").unwrap(),
                item.get_string("id").unwrap(),
                item.get_int("count").unwrap(),
            )
        })
        .collect();
    assert_eq!(
        actual,
        [
            (0, "minecraft:iron_ingot", 19),
            (26, "minecraft:diamond", 1)
        ]
    );
}

#[test]
fn converts_a_saved_column_without_writing_to_the_source() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("bedrock");
    let destination = directory.path().join("java");
    write_world(&source, false, FLAT_LAYERS);
    let before = snapshot(&source);
    let stats = import_terrain(&source, &destination).unwrap();
    assert_eq!((stats.chunks, stats.regions), (1, 1));
    assert_eq!(snapshot(&source), before);

    let pos = Vector2::new(-1, 0);
    let region_pos = region_of(pos);
    let region = AnvilRegion::open(
        &destination
            .join("region")
            .join(AnvilRegion::file_name(region_pos)),
        region_pos,
    )
    .unwrap();
    let chunk_bytes =
        AnvilRegion::decode_chunk(region.read_chunk(region_chunk_index(pos)).unwrap().unwrap())
            .unwrap();
    let chunk = ChunkData::internal_from_bytes(&chunk_bytes, pos).unwrap();
    assert_eq!(
        chunk.section.get_block_absolute_y(13, -60, 3),
        Some(Block::DIAMOND_BLOCK.default_state.id)
    );
    assert_eq!(
        chunk.section.get_block_absolute_y(13, -59, 3),
        Some(Block::WATER.default_state.id)
    );
    assert_eq!(
        chunk.section.get_block_absolute_y(3, -60, 13),
        Some(BlockStateId::AIR)
    );
    assert_eq!(
        chunk.section.get_rough_biome_absolute_y(13, -60, 3),
        Some(Biome::PLAINS.id)
    );
    let info = AnvilLevelInfo.read_world_info(&destination).unwrap();
    assert_eq!(info.world_gen_settings.seed, -764);
    assert_eq!((info.spawn_x, info.spawn_y, info.spawn_z), (-3, -59, 3));
    let flat = info.world_gen_settings.dimensions[OVERWORLD.minecraft_name]
        .generator
        .settings
        .as_ref()
        .and_then(GeneratorSettings::as_flat_settings)
        .unwrap();
    assert_eq!(
        flat.layers,
        [FlatPresetLayer {
            block: "minecraft:stone".to_owned(),
            height: 3
        }]
    );
    let error = import_terrain(&source, &destination).unwrap_err();
    assert!(error.to_string().contains("destination already exists"));
}

#[test]
fn malformed_block_entities_leave_no_destination() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("bedrock");
    let destination = directory.path().join("java");
    write_world(&source, false, FLAT_LAYERS);
    let db = DB::open(source.join("db"), Options::default()).unwrap();
    let mut key = (-1i32).to_le_bytes().to_vec();
    key.extend_from_slice(&0i32.to_le_bytes());
    key.push(BLOCK_ENTITY);
    db.put(&key, &[pumpkin_nbt::COMPOUND_ID, 0]).unwrap();
    db.close().unwrap();
    let before = snapshot(&source);
    let error = import_terrain(&source, &destination).unwrap_err();
    assert!(error.to_string().contains("Bedrock NBT"));
    assert!(!destination.exists());
    assert_eq!(snapshot(&source), before);
}

#[test]
fn unmapped_terrain_leaves_no_destination() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("bedrock");
    let destination = directory.path().join("java");
    write_world(&source, true, FLAT_LAYERS);
    let db = DB::open(source.join("db"), Options::default()).unwrap();
    let mut key = [0; 9];
    key[8] = VERSION;
    db.put(&key, &[CHUNK_VERSION - 1]).unwrap();
    db.close().unwrap();
    let before = snapshot(&source);
    let inspection = inspect_terrain(&source, GeneratorPolicy::RequireSource).unwrap();
    assert_eq!(inspection.blockers, 2);
    assert_eq!(inspection.stats.chunks, 0);
    assert!(
        inspection.blocker_examples[0]
            .to_string()
            .contains("example:unmapped")
    );
    assert!(
        inspection.blocker_examples[1]
            .to_string()
            .contains("unsupported Bedrock chunk version")
    );
    let error = import_terrain(&source, &destination).unwrap_err();
    assert!(error.to_string().contains("example:unmapped"));
    assert!(!destination.exists());
    assert_eq!(snapshot(&source), before);
}

#[test]
fn missing_generator_requires_an_explicit_policy_for_new_chunks() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("bedrock");
    let destination = directory.path().join("java");
    write_world(&source, false, "");
    let before = snapshot(&source);

    let inspection = inspect_terrain(&source, GeneratorPolicy::RequireSource).unwrap();
    assert_eq!((inspection.blockers, inspection.stats.chunks), (1, 1));
    assert!(
        inspection.blocker_examples[0]
            .to_string()
            .contains("--new-chunks=normal")
    );
    assert!(import_terrain(&source, &destination).is_err());
    assert!(!destination.exists());
    assert_eq!(snapshot(&source), before);

    let policy = GeneratorPolicy::NormalIfUnspecified;
    let inspection = inspect_terrain(&source, policy).unwrap();
    assert_eq!((inspection.blockers, inspection.stats.chunks), (0, 1));
    assert!(inspection.stats.default_generator_selected);
    let stats = import_terrain_with_generator(&source, &destination, policy).unwrap();
    assert!(stats.default_generator_selected);
    let info = AnvilLevelInfo.read_world_info(&destination).unwrap();
    assert_eq!(info.world_gen_settings.seed, -764);
    assert_eq!(
        info.world_gen_settings.dimensions[OVERWORLD.minecraft_name]
            .generator
            .generator_type,
        "minecraft:noise"
    );
    assert_eq!(snapshot(&source), before);
}
