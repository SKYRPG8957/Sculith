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
    biomes.extend(std::iter::repeat_n(
        0xff,
        SavedDimension::Overworld.disk_sections() - 1,
    ));
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
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            files.extend(snapshot(&path));
        } else {
            let content = fs::read(&path).unwrap();
            files.insert(path, content);
        }
    }
    files
}

fn string_tag(nbt: &mut Vec<u8>, key: &str, value: &str) {
    let mut bytes = u16::try_from(value.len()).unwrap().to_le_bytes().to_vec();
    bytes.extend_from_slice(value.as_bytes());
    named_tag(nbt, pumpkin_nbt::STRING_ID, key, &bytes);
}

fn container(nbt: &mut Vec<u8>, kind: &str, x: i32, items: &[(u8, &str, u8)]) {
    container_at(nbt, kind, BlockPos::new(x, -60, 0), items);
}

fn container_at(nbt: &mut Vec<u8>, kind: &str, pos: BlockPos, items: &[(u8, &str, u8)]) {
    nbt.extend_from_slice(&[pumpkin_nbt::COMPOUND_ID, 0, 0]);
    string_tag(nbt, "id", kind);
    for (name, value) in [("x", pos.0.x), ("y", pos.0.y), ("z", pos.0.z)] {
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
        &[
            (0, "minecraft:apple", 3),
            (1, "minecraft:unmapped_fixture_item", 1),
        ],
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
    let unmapped = &stats.skipped_block_entity_examples[1];
    assert_eq!(unmapped.position, Some(BlockPos::new(-3, -60, 0)));
    assert!(unmapped.reason.contains("unmapped_fixture_item"));
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

fn bedrock_storage_item(name: &str, damage: i16) -> NbtCompound {
    let mut entry = NbtCompound::new();
    entry.put_string("Name", format!("minecraft:{name}"));
    entry.put_byte("Count", 1);
    entry.put_short("Damage", damage);
    entry
}

fn round_trip_storage_item(
    source: &NbtCompound,
    terrain: &terrain::TerrainRegistry,
    items: &storage::StorageItemRegistry,
) -> (pumpkin_data::item_stack::ItemStack, NbtCompound) {
    let stack = storage::convert_stack(source, terrain, items).unwrap();
    let mut saved = NbtCompound::new();
    stack.write_item_stack(&mut saved);
    let decoded = storage::read_sculith_stack(&saved, items).unwrap();
    assert!(stack.are_equal(&decoded));
    let mut exported = storage::new_bedrock_stack(&decoded, items).unwrap();
    exported.put_byte("Count", 1);
    assert!(
        storage::convert_stack(&exported, terrain, items)
            .unwrap()
            .are_equal(&decoded)
    );
    (decoded, exported)
}

#[test]
fn storage_item_versions_guard_potion_and_enchantment_round_trips() {
    use pumpkin_data::{
        Enchantment,
        data_component_impl::{EnchantmentsImpl, PotionContentsImpl, StoredEnchantmentsImpl},
        potion::Potion,
    };

    let directory = tempfile::tempdir().unwrap();
    let legacy = storage::read_import_registry(directory.path()).unwrap();
    std::fs::write(
        directory.path().join(".sculith-bedrock-storage-version"),
        b"2\n",
    )
    .unwrap();
    let version_two = storage::read_import_registry(directory.path()).unwrap();
    storage::write_item_version(directory.path()).unwrap();
    let items = storage::read_import_registry(directory.path()).unwrap();
    let terrain = terrain::TerrainRegistry::load().unwrap();
    for (name, aux, potion) in [
        ("potion", 0, &Potion::WATER),
        ("splash_potion", 16, &Potion::STRONG_SWIFTNESS),
        ("lingering_potion", 46, &Potion::INFESTED),
    ] {
        let source = bedrock_storage_item(name, aux);
        assert!(storage::convert_stack(&source, &terrain, &legacy).is_err());
        let (decoded, exported) = round_trip_storage_item(&source, &terrain, &items);
        assert!(
            storage::convert_stack(&source, &terrain, &version_two)
                .unwrap()
                .are_equal(&decoded)
        );
        let contents = decoded.get_data_component::<PotionContentsImpl>().unwrap();
        assert_eq!(contents.potion_id, Some(i32::from(potion.id)));
        assert_eq!(exported.get_short("Damage"), Some(aux));
    }
    for aux in [2, 47] {
        assert!(
            storage::convert_stack(&bedrock_storage_item("potion", aux), &terrain, &items).is_err()
        );
    }

    let enchantment = |id, level| {
        let mut tag = NbtCompound::new();
        tag.put_short("id", id);
        tag.put_short("lvl", level);
        NbtTag::Compound(tag)
    };
    let mut gear = bedrock_storage_item("diamond_chestplate", 0);
    let mut tag = NbtCompound::new();
    tag.put_list("ench", vec![enchantment(17, 3), enchantment(0, 4)]);
    gear.put_compound("tag", tag);
    let (decoded, _) = round_trip_storage_item(&gear, &terrain, &items);
    let entries = &decoded
        .get_data_component::<EnchantmentsImpl>()
        .unwrap()
        .enchantment;
    assert!(entries.contains(&(&Enchantment::PROTECTION, 4)));
    assert!(entries.contains(&(&Enchantment::UNBREAKING, 3)));
    assert!(storage::convert_stack(&gear, &terrain, &legacy).is_err());

    let mut book = bedrock_storage_item("enchanted_book", 0);
    let mut tag = NbtCompound::new();
    tag.put_list("ench", vec![enchantment(26, 1)]);
    book.put_compound("tag", tag);
    let (decoded, _) = round_trip_storage_item(&book, &terrain, &items);
    let stored = &decoded
        .get_data_component::<StoredEnchantmentsImpl>()
        .unwrap()
        .enchantment;
    assert_eq!(stored.len(), 1);
    assert!(stored.contains(&(&Enchantment::MENDING, 1)));
}

#[test]
fn storage_decay_stays_out_of_older_exports() {
    let directory = tempfile::tempdir().unwrap();
    let legacy = storage::read_import_registry(directory.path()).unwrap();
    std::fs::write(
        directory.path().join(".sculith-bedrock-storage-version"),
        b"2\n",
    )
    .unwrap();
    let version_two = storage::read_import_registry(directory.path()).unwrap();
    storage::write_item_version(directory.path()).unwrap();
    let items = storage::read_import_registry(directory.path()).unwrap();
    let terrain = terrain::TerrainRegistry::load().unwrap();
    let decay = bedrock_storage_item("potion", 36);
    assert!(storage::convert_stack(&decay, &terrain, &legacy).is_err());
    assert!(storage::convert_stack(&decay, &terrain, &version_two).is_err());
    let (decoded, exported) = round_trip_storage_item(&decay, &terrain, &items);
    assert_eq!(exported.get_short("Damage"), Some(36));

    let mut saved = NbtCompound::new();
    decoded.write_item_stack(&mut saved);
    assert!(storage::read_sculith_stack(&saved, &version_two).is_err());
    let mut components = saved.get_compound("components").unwrap().clone();
    let mut potion = components
        .get_compound("minecraft:potion_contents")
        .unwrap()
        .clone();
    let mut effect = potion.get_list("custom_effects").unwrap()[0]
        .extract_compound()
        .unwrap()
        .clone();
    effect.put_int("unknown_effect_field", 1);
    potion.put_list("custom_effects", vec![NbtTag::Compound(effect)]);
    components.put_compound("minecraft:potion_contents", potion);
    saved.put_compound("components", components);
    assert!(storage::read_sculith_stack(&saved, &items).is_err());
}

#[test]
fn storage_tipped_arrows_keep_native_variants_and_original_export_scope() {
    use pumpkin_data::item::Item;

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join(".sculith-bedrock-storage-version"),
        b"3\n",
    )
    .unwrap();
    let earlier = storage::read_import_registry(directory.path()).unwrap();
    storage::write_item_version(directory.path()).unwrap();
    let items = storage::read_import_registry(directory.path()).unwrap();
    let terrain = terrain::TerrainRegistry::load().unwrap();

    for aux in [15, 26, 37, 47] {
        let source = bedrock_storage_item("arrow", aux);
        assert!(storage::convert_stack(&source, &terrain, &earlier).is_err());
        let (tipped, encoded) = round_trip_storage_item(&source, &terrain, &items);
        assert_eq!(tipped.item, &Item::TIPPED_ARROW);
        assert_eq!(encoded.get_short("Damage"), Some(aux));
        let mut saved = NbtCompound::new();
        tipped.write_item_stack(&mut saved);
        assert!(storage::read_sculith_stack(&saved, &earlier).is_err());
    }
    let source = bedrock_storage_item("arrow", 0);
    let (ordinary, encoded) = round_trip_storage_item(&source, &terrain, &items);
    assert_eq!(ordinary.item, &Item::ARROW);
    assert_eq!(encoded.get_short("Damage"), Some(0));
    let decay = bedrock_storage_item("potion", 36);
    assert!(storage::convert_stack(&decay, &terrain, &earlier).is_ok());
}

#[test]
fn storage_export_replaces_slots_and_keeps_unsupported_compound_bytes() {
    let pos = BlockPos::new(-1, -60, 0);
    let mut raw = Vec::new();
    container(&mut raw, "Chest", -1, &[(0, "minecraft:iron_ingot", 19)]);
    let boundary = raw.len();
    container(&mut raw, "Furnace", -2, &[]);
    let terrain = terrain::TerrainRegistry::load().unwrap();
    let items = storage::item_registry();
    let mut original = ChunkData::empty(-1, 0);
    original
        .section
        .set_block_absolute_y(15, -60, 0, Block::CHEST.default_state.id);
    storage::import_record(
        &raw,
        &mut original,
        &terrain,
        &items,
        &mut ImportStats::default(),
    )
    .unwrap();

    let mut shared = ChunkData::empty(-1, 0);
    shared
        .section
        .set_block_absolute_y(15, -60, 0, Block::CHEST.default_state.id);
    let mut entity = original.pending_block_entities.get_mut().unwrap()[&pos].clone();
    let mut entry = pumpkin_nbt::compound::NbtCompound::new();
    entry.put_byte("Slot", 8);
    entry.put_string("id", "minecraft:diamond".into());
    entry.put_int("count", 3);
    entity.put_list("Items", vec![pumpkin_nbt::tag::NbtTag::Compound(entry)]);
    shared
        .pending_block_entities
        .get_mut()
        .unwrap()
        .insert(pos, entity);

    let (exported, edited) =
        storage_export::export_record(&raw, &original, &shared, &terrain, &items).unwrap();
    assert_eq!(edited, 1);
    assert!(exported.ends_with(&raw[boundary..]));
    let mut reader = NbtReadHelperBedrockDisk::new(Cursor::new(exported.as_slice()));
    let chest = Nbt::read(&mut reader).unwrap();
    let stacks = chest.get_list("Items").unwrap();
    assert_eq!(stacks.len(), 1);
    let stack = stacks[0].extract_compound().unwrap();
    assert_eq!(stack.get_string("Name"), Some("minecraft:diamond"));
    assert_eq!(stack.get_byte("Slot"), Some(8));
    assert_eq!(stack.get_byte("Count"), Some(3));
    assert_eq!(stack.get_short("Damage"), Some(0));
    assert_eq!(
        Nbt::read(&mut reader).unwrap().get_string("id"),
        Some("Furnace")
    );
}

fn write_storage_column(db: &DB, position: BlockPos, id: i32, count: u8) -> Vec<u8> {
    let mut key = position.0.x.div_euclid(16).to_le_bytes().to_vec();
    key.extend_from_slice(&position.0.z.div_euclid(16).to_le_bytes());
    if id != 0 {
        key.extend_from_slice(&id.to_le_bytes());
    }
    key.push(VERSION);
    db.put(&key, &[CHUNK_VERSION]).unwrap();
    let tag = key.len() - 1;
    let dimension = SavedDimension::from_disk(id).unwrap();
    key[tag] = BIOMES;
    let mut biomes = vec![0; 512];
    biomes.push(1);
    biomes.extend_from_slice(&u32::from(Biome::PLAINS.be_network_id).to_le_bytes());
    biomes.extend(std::iter::repeat_n(0xff, dimension.disk_sections() - 1));
    db.put(&key, &biomes).unwrap();
    let height = position.0.y.div_euclid(16) as u8;
    key[tag] = SUBCHUNK;
    key.push(height);
    let mut states = Vec::new();
    named_tag(
        &mut states,
        pumpkin_nbt::INT_ID,
        "facing_direction",
        &2i32.to_le_bytes(),
    );
    named_tag(&mut states, pumpkin_nbt::BYTE_ID, "open_bit", &[0]);
    states.push(END_ID);
    let mut section = vec![9, 1, height, 0];
    block_entry(&mut section, "minecraft:barrel", &states);
    db.put(&key, &section).unwrap();
    key.pop();
    key[tag] = BLOCK_ENTITY;
    let mut record = Vec::new();
    container_at(
        &mut record,
        "Barrel",
        position,
        &[(4, "minecraft:iron_ingot", count)],
    );
    db.put(&key, &record).unwrap();
    key
}

#[test]
fn storage_export_routes_same_coordinates_to_each_dimension() {
    use crate::chunk::io::converter::convert_world;
    use sculith_config::chunk::ChunkConfig;

    let directory = tempfile::tempdir().unwrap();
    let template = directory.path().join("bedrock");
    let shared = directory.path().join("sculith");
    let position = BlockPos::new(-1, 0, 0);
    let chunk_pos = Vector2::new(-1, 0);
    let region_pos = region_of(chunk_pos);
    write_world(&template, false, FLAT_LAYERS);
    let database = DB::open(template.join("db"), Options::default()).unwrap();
    let records = [(0, 1), (1, 2), (2, 3)]
        .map(|(id, count)| write_storage_column(&database, position, id, count));
    database.close().unwrap();
    let before = snapshot(&template);
    assert_eq!(
        import_terrain(&template, &shared)
            .unwrap()
            .containers_imported,
        3
    );
    for (id, dimension) in [
        SavedDimension::Overworld,
        SavedDimension::Nether,
        SavedDimension::End,
    ]
    .into_iter()
    .enumerate()
    {
        let folder = LevelFolder::for_dimension(shared.clone(), &dimension.target());
        let path = folder
            .region_folder
            .join(AnvilRegion::file_name(region_pos));
        let mut region = AnvilRegion::open(&path, region_pos).unwrap();
        let index = region_chunk_index(chunk_pos);
        let raw = region.read_chunk(index).unwrap().unwrap();
        let bytes = AnvilRegion::decode_chunk(raw).unwrap();
        let mut chunk = ChunkData::internal_from_bytes(&bytes, chunk_pos).unwrap();
        let entity = chunk
            .pending_block_entities
            .get_mut()
            .unwrap()
            .get_mut(&position)
            .unwrap();
        let mut item = entity.get_list("Items").unwrap()[0]
            .extract_compound()
            .unwrap()
            .clone();
        item.put_int("count", 10 + id as i32);
        entity.put_list("Items", vec![pumpkin_nbt::tag::NbtTag::Compound(item)]);
        let encoded =
            AnvilRegion::encode_chunk(chunk.internal_to_bytes(), &AnvilChunkConfig::default())
                .unwrap();
        region.stage_chunk(index, encoded);
        region.flush(true).unwrap();
    }
    for (name, format) in [
        ("anvil", ChunkConfig::Anvil(AnvilChunkConfig::default())),
        ("pump", ChunkConfig::Pump),
        ("linear", ChunkConfig::Linear),
    ] {
        convert_world(&shared, &format, true).unwrap();
        let shared_before = snapshot(&shared);
        let exported = directory.path().join(name);
        let stats = storage_export::export_storage(&template, &shared, &exported).unwrap();
        assert_eq!(
            (stats.containers_checked, stats.inventories_updated),
            (3, 3)
        );
        assert_eq!(snapshot(&template), before);
        assert_eq!(snapshot(&shared), shared_before);
        let database = DB::open(exported.join("db"), bedrock_db_options()).unwrap();
        for (id, key) in records.iter().enumerate() {
            let data = database.try_get(key).unwrap().unwrap();
            let mut reader = NbtReadHelperBedrockDisk::new(Cursor::new(data));
            let barrel = Nbt::read(&mut reader).unwrap();
            let item = barrel.get_list("Items").unwrap()[0]
                .extract_compound()
                .unwrap();
            assert_eq!(item.get_byte("Slot"), Some(4));
            assert_eq!(item.get_byte("Count"), Some(10 + id as i8));
        }
    }
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
fn distinguishes_saved_dimensions_at_the_same_coordinates() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("bedrock");
    let destination = directory.path().join("java");
    write_world(&source, false, FLAT_LAYERS);
    let scratch_dimensions = [
        (
            1i32,
            &Biome::NETHER_WASTES,
            &Block::GOLD_BLOCK,
            "minecraft:gold_block",
            8,
        ),
        (
            2,
            &Biome::THE_END,
            &Block::EMERALD_BLOCK,
            "minecraft:emerald_block",
            16,
        ),
    ];
    let db = DB::open(source.join("db"), Options::default()).unwrap();
    for (dimension, biome, _, name, sections) in scratch_dimensions {
        let mut key = (-1i32).to_le_bytes().to_vec();
        key.extend_from_slice(&0i32.to_le_bytes());
        key.extend_from_slice(&dimension.to_le_bytes());
        key.push(VERSION);
        db.put(&key, &[CHUNK_VERSION]).unwrap();
        key[12] = BIOMES;
        let mut biomes = vec![0; 512];
        biomes.push(1);
        biomes.extend_from_slice(&u32::from(biome.be_network_id).to_le_bytes());
        biomes.extend(std::iter::repeat_n(0xff, sections - 1));
        db.put(&key, &biomes).unwrap();
        key[12] = SUBCHUNK;
        key.push(0);
        let mut terrain = vec![9, 1, 0, 0];
        block_entry(&mut terrain, name, &[0]);
        db.put(&key, &terrain).unwrap();
        if dimension == 2 {
            key[13] = 1;
            let mut states = Vec::new();
            named_tag(&mut states, pumpkin_nbt::BYTE_ID, "infiniburn_bit", &[1]);
            states.push(END_ID);
            let mut platform = vec![9, 1, 1, 0];
            block_entry(&mut platform, "minecraft:bedrock", &states);
            db.put(&key, &platform).unwrap();
        }
    }
    db.close().unwrap();
    let before = snapshot(&source);
    let inspection = inspect_terrain(&source, GeneratorPolicy::RequireSource).unwrap();
    assert_eq!(inspection.blockers, 0, "{:?}", inspection.blocker_examples);
    assert_eq!((inspection.stats.chunks, inspection.stats.regions), (3, 3));
    let stats = import_terrain(&source, &destination).unwrap();
    assert_eq!((stats.chunks, stats.regions), (3, 3));
    assert_eq!(snapshot(&source), before);

    let pos = Vector2::new(-1, 0);
    let region_pos = region_of(pos);
    for (id, biome, expected, _, _) in scratch_dimensions {
        let dimension = SavedDimension::from_disk(id).unwrap().target();
        let folders = LevelFolder::for_dimension(destination.clone(), &dimension);
        let path = folders
            .region_folder
            .join(AnvilRegion::file_name(region_pos));
        assert!(path.is_file());
        let region = AnvilRegion::open_read_only(&path, region_pos).unwrap();
        let raw = region.read_chunk(region_chunk_index(pos)).unwrap().unwrap();
        let bytes = AnvilRegion::decode_chunk(raw).unwrap();
        let chunk = ChunkData::internal_from_bytes(&bytes, pos).unwrap();
        assert_eq!(
            chunk.section.get_block_absolute_y(0, 0, 0),
            Some(expected.default_state.id)
        );
        assert_eq!(
            chunk.section.get_block_absolute_y(0, 128, 0),
            Some(BlockStateId::AIR)
        );
        assert_eq!(
            chunk.section.get_rough_biome_absolute_y(0, 128, 0),
            Some(biome.id)
        );
        if id == 2 {
            assert_eq!(
                chunk.section.get_block_absolute_y(0, 16, 0),
                Some(Block::BEDROCK.default_state.id)
            );
        }
    }
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

fn write_livestock_fixture(source: &Path, pos: Vector2<i32>) {
    use pumpkin_nbt::{compound::NbtCompound, tag::NbtTag};

    write_world(source, false, FLAT_LAYERS);
    let db = DB::open(source.join("db"), Options::default()).unwrap();

    let mut record = NbtCompound::new();
    record.put_string("identifier", "minecraft:pig".into());
    record.put_long("UniqueID", -101);
    record.put_list(
        "Pos",
        vec![(-2.5f32).into(), (-60f32).into(), 3.5f32.into()],
    );
    record.put_list("Rotation", vec![90f32.into(), 0f32.into()]);
    record.put_int("Age", -450);
    record.put_bool("IsBaby", true);
    record.put_bool("Persistent", true);
    record.put_string("CustomName", "{새끼 돼지".into());
    let mut health = NbtCompound::new();
    health.put_string("Name", "minecraft:health".into());
    health.put_float("Base", 10.0);
    health.put_float("Current", 3.0);
    record.put_list("Attributes", vec![NbtTag::Compound(health)]);

    let put = |id: [u8; 8], bytes: Vec<u8>| {
        let mut key = actors::ACTOR_PREFIX.to_vec();
        key.extend_from_slice(&id);
        db.put(&key, &bytes).unwrap();
    };
    let encode = |record: NbtCompound| {
        let mut bytes = Vec::new();
        Nbt::from(record)
            .write_to_writer_bedrock_disk(&mut bytes)
            .unwrap();
        bytes
    };
    let key = [0, 0, 0, 1, 0, 0, 0, 243];
    let mut bytes = encode(record.clone());
    assert_eq!(bytes.pop(), Some(pumpkin_nbt::END_ID));
    let mut component = Vec::new();
    named_tag(
        &mut component,
        pumpkin_nbt::STRING_ID,
        "StorageKey",
        &[8, 0, 0, 0, 0, 1, 0, 0, 0, 243],
    );
    component.push(pumpkin_nbt::END_ID);
    let mut internal = Vec::new();
    named_tag(
        &mut internal,
        pumpkin_nbt::COMPOUND_ID,
        "EntityStorageKeyComponent",
        &component,
    );
    internal.push(pumpkin_nbt::END_ID);
    named_tag(
        &mut bytes,
        pumpkin_nbt::COMPOUND_ID,
        "internalComponents",
        &internal,
    );
    bytes.push(pumpkin_nbt::END_ID);
    put(key, bytes);
    let linked = [0, 0, 0, 1, 0, 0, 0, 244];
    let mut attached = record.clone();
    attached.put_long("UniqueID", -102);
    attached.put_long("LeasherID", 42);
    put(linked, encode(attached));
    let rider = [0, 0, 0, 1, 0, 0, 0, 246];
    let mut riding = record.clone();
    riding.put_long("UniqueID", -103);
    put(rider, encode(riding));
    let vehicle = [0, 0, 0, 1, 0, 0, 0, 247];
    let mut boat = NbtCompound::new();
    boat.put_string("identifier", "minecraft:boat".into());
    let mut link = NbtCompound::new();
    link.put_long("entityID", -103);
    boat.put_list("LinksTag", vec![NbtTag::Compound(link)]);
    put(vehicle, encode(boat));
    let elsewhere = [0, 0, 0, 1, 0, 0, 0, 245];
    record.put_long("UniqueID", -104);
    record.put_list("Pos", vec![2.5f32.into(), (-60f32).into(), 3.5f32.into()]);
    put(elsewhere, encode(record));

    let mut index = b"digp".to_vec();
    index.extend_from_slice(&pos.x.to_le_bytes());
    index.extend_from_slice(&pos.y.to_le_bytes());
    db.put(
        &index,
        &[key, key, linked, elsewhere, rider, vehicle].concat(),
    )
    .unwrap();
    db.close().unwrap();
}

#[test]
fn imported_livestock_is_indexed_once_and_writes_the_existing_entity_format() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("bedrock");
    let pos = Vector2::new(-1, 0);
    write_livestock_fixture(&source, pos);
    let before = snapshot(&source);
    let inspection = inspect_terrain(&source, GeneratorPolicy::RequireSource).unwrap();
    assert_eq!(inspection.blockers, 0);
    assert_eq!(inspection.stats.entities_imported, 1);
    assert_eq!(inspection.stats.entity_records_excluded, 4);
    assert_eq!(inspection.stats.skipped_actor_examples.len(), 4);
    let read = |name: &str| {
        let destination = directory.path().join(name);
        let stats = import_terrain(&source, &destination).unwrap();
        assert_eq!(
            (stats.entities_imported, stats.entity_records_excluded),
            (1, 4)
        );
        let region_pos = region_of(pos);
        let region = AnvilRegion::open(
            &destination
                .join("entities")
                .join(AnvilRegion::file_name(region_pos)),
            region_pos,
        )
        .unwrap();
        let bytes =
            AnvilRegion::decode_chunk(region.read_chunk(region_chunk_index(pos)).unwrap().unwrap())
                .unwrap();
        <ChunkEntityData as crate::chunk::format::anvil::SingleChunkDataSerializer>::from_bytes(
            &bytes, pos,
        )
        .unwrap()
        .data
        .into_inner()
        .unwrap()
    };
    let first = read("java");
    assert_eq!(first, read("java-again"));
    assert_eq!(first.len(), 1);
    let imported = &first[0];
    assert_eq!(imported.get_string("id"), Some("minecraft:pig"));
    assert_eq!(imported.get_int("Age"), Some(-450));
    assert_eq!(imported.get_float("Health"), Some(3.0));
    assert_eq!(imported.get_bool("PersistenceRequired"), Some(true));
    assert_eq!(
        imported
            .get_compound("CustomName")
            .unwrap()
            .get_string("text"),
        Some("{새끼 돼지")
    );
    assert!(imported.get_uuid("UUID").is_some());
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

fn write_player_fixture(source: &Path, key: &str, body_key: &str, dimensions: [i32; 2]) {
    let mut alias = vec![COMPOUND_ID, 0, 0];
    string_tag(&mut alias, "ServerId", body_key);
    alias.push(END_ID);

    let mut body = vec![COMPOUND_ID, 0, 0];
    string_tag(&mut body, "identifier", "minecraft:player");
    for (name, value) in [
        ("DimensionId", dimensions[0]),
        ("PlayerGameMode", 0),
        ("PlayerLevel", 4),
        ("SelectedInventorySlot", 0),
        ("SpawnX", -3),
        ("SpawnY", if dimensions[1] == 0 { -59 } else { 65 }),
        ("SpawnZ", 3),
        ("SpawnDimension", dimensions[1]),
        ("SpawnBlockPositionX", i32::MIN),
    ] {
        named_tag(&mut body, pumpkin_nbt::INT_ID, name, &value.to_le_bytes());
    }
    named_tag(
        &mut body,
        pumpkin_nbt::FLOAT_ID,
        "PlayerLevelProgress",
        &(7.0f32 / 15.0).to_le_bytes(),
    );
    let position = [-3.5, if dimensions[0] == 0 { -57.38 } else { 66.62 }, 3.5];
    for (name, values) in [
        ("Pos", position.as_slice()),
        ("Rotation", [47.0, -18.0].as_slice()),
    ] {
        let mut list = vec![pumpkin_nbt::FLOAT_ID];
        list.extend_from_slice(&u32::try_from(values.len()).unwrap().to_le_bytes());
        for &value in values {
            list.extend_from_slice(&f32::to_le_bytes(value));
        }
        named_tag(&mut body, pumpkin_nbt::LIST_ID, name, &list);
    }
    let mut inventory = vec![COMPOUND_ID, 1, 0, 0, 0];
    string_tag(&mut inventory, "Name", "minecraft:potion");
    named_tag(
        &mut inventory,
        pumpkin_nbt::SHORT_ID,
        "Damage",
        &14i16.to_le_bytes(),
    );
    named_tag(&mut inventory, pumpkin_nbt::BYTE_ID, "Slot", &[12]);
    named_tag(&mut inventory, pumpkin_nbt::BYTE_ID, "Count", &[1]);
    inventory.push(END_ID);
    named_tag(&mut body, pumpkin_nbt::LIST_ID, "Inventory", &inventory);
    body.push(END_ID);

    let db = DB::open(source.join("db"), Options::default()).unwrap();
    db.put(key.as_bytes(), &alias).unwrap();
    db.put(body_key.as_bytes(), &body).unwrap();
    db.close().unwrap();
}

#[test]
fn imports_a_selected_player_and_rejects_duplicate_player_links() {
    use crate::data::player_data::PlayerDataStorage;
    use uuid::Uuid;

    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("bedrock");
    write_world(&source, false, FLAT_LAYERS);
    let key = "player_0123456";
    let body_key = "player_server_eee37dc7-41a2-4e3f-9265-8877db158165";
    let player = Uuid::from_u128(1);
    write_player_fixture(&source, key, body_key, [0, 0]);
    let untouched = snapshot(&source);

    let map = temp.path().join("map.json");
    let selected = serde_json::json!({"source": key, "uuid": player});
    fs::write(&map, serde_json::to_vec(&[&selected]).unwrap()).unwrap();
    let options = ImportOptions {
        generator: GeneratorPolicy::RequireSource,
        player_map: Some(&map),
    };
    let report = inspect_world(&source, options).unwrap();
    assert_eq!(report.blockers, 0);
    assert_eq!(report.stats.players_imported, 1);
    let output = temp.path().join("sculith");
    assert_eq!(
        import_world(&source, &output, options)
            .unwrap()
            .players_imported,
        1
    );
    let data = PlayerDataStorage::new(output.join("players/data"), true);
    let (found, record) = data.load_player_data(&player).unwrap();
    assert!(found);
    assert_eq!(record.get_int("XpTotal"), Some(47));
    let position = record.get_list("Pos").unwrap();
    assert!((position[1].extract_double().unwrap() + 59.0).abs() < 0.00001);
    let inventory = record.get_list("Inventory").unwrap();
    let potion = inventory[0].extract_compound().unwrap();
    assert_eq!(potion.get_byte("Slot"), Some(12));
    assert_eq!(potion.get_string("id"), Some("minecraft:potion"));
    let respawn = record.get_compound("respawn").unwrap();
    assert_eq!(
        respawn.get("pos"),
        Some(&NbtTag::IntArray(vec![-3, -59, 3]))
    );

    let repeated = serde_json::json!({"source": body_key, "uuid": Uuid::from_u128(2)});
    fs::write(&map, serde_json::to_vec(&[selected, repeated]).unwrap()).unwrap();
    let report = inspect_world(&source, options).unwrap();
    assert_eq!(report.blockers, 1);
    assert!(
        report.blocker_examples[0]
            .to_string()
            .contains("repeated Bedrock player record")
    );
    let output = temp.path().join("duplicate");
    assert!(import_world(&source, &output, options).is_err());
    assert!(!output.exists());
    assert_eq!(snapshot(&source), untouched);
}

#[test]
fn imports_players_with_separate_saved_and_respawn_dimensions() {
    use crate::data::player_data::PlayerDataStorage;
    use uuid::Uuid;

    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("bedrock");
    write_world(&source, false, FLAT_LAYERS);
    let key = "player_0123456";
    let body_key = "player_server_eee37dc7-41a2-4e3f-9265-8877db158165";
    write_player_fixture(&source, key, body_key, [2, 1]);
    let player = Uuid::from_u128(1);
    let map = temp.path().join("map.json");
    let selected = serde_json::json!([{"source": key, "uuid": player}]);
    fs::write(&map, serde_json::to_vec(&selected).unwrap()).unwrap();
    let output = temp.path().join("sculith");
    let options = ImportOptions {
        generator: GeneratorPolicy::RequireSource,
        player_map: Some(&map),
    };
    assert_eq!(
        import_world(&source, &output, options)
            .unwrap()
            .players_imported,
        1
    );
    let data = PlayerDataStorage::new(output.join("players/data"), true);
    let (found, record) = data.load_player_data(&player).unwrap();
    assert!(found);
    assert_eq!(record.get_string("Dimension"), Some("minecraft:the_end"));
    let respawn = record.get_compound("respawn").unwrap();
    assert_eq!(
        respawn.get_string("dimension"),
        Some("minecraft:the_nether")
    );
    assert_eq!(respawn.get("pos"), Some(&NbtTag::IntArray(vec![-3, 65, 3])));
}
