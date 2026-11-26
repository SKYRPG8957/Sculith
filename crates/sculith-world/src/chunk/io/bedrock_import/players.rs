use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
    path::Path,
};

use leveldb::DB;
use pumpkin_data::{data_component_impl::DamageImpl, entity::EntityType, item_stack::ItemStack};
use pumpkin_nbt::{compound::NbtCompound, tag::NbtTag};
use sculith_util::{GameMode, math::experience};
use serde::Deserialize;
use uuid::Uuid;

use super::{
    CHUNK_COORDINATE_LIMIT, Error, ImportStats, SavedDimension, finite_floats, invalid,
    read_record,
    storage::{StorageItemRegistry, convert_stack},
    terrain::TerrainRegistry,
};
use crate::world_info::MAXIMUM_SUPPORTED_WORLD_DATA_VERSION;

const SERVER_PREFIX: &str = "player_server_";
const MAX_MAP_BYTES: u64 = 1024 * 1024;
const MAX_PLAYER_BYTES: usize = 256 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlayerLink {
    source: String,
    uuid: Uuid,
}

pub(super) fn source_key(key: &[u8]) -> Option<&str> {
    let key = std::str::from_utf8(key).ok()?;
    let id = key
        .strip_prefix(SERVER_PREFIX)
        .or_else(|| key.strip_prefix("player_"))?;
    (!id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-'))
    .then_some(key)
}

pub(super) fn read_selected(
    db: &DB,
    map: Option<&Path>,
    terrain: &TerrainRegistry,
    items: &StorageItemRegistry,
    stats: &mut ImportStats,
) -> Result<BTreeMap<Uuid, NbtCompound>, Error> {
    let Some(map) = map else {
        return Ok(BTreeMap::new());
    };
    let mut bytes = Vec::new();
    File::open(map)?
        .take(MAX_MAP_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MAP_BYTES {
        return Err(invalid("Bedrock player map exceeds 1 MiB"));
    }
    let links: Vec<PlayerLink> = serde_json::from_slice(&bytes)
        .map_err(|error| invalid(format!("invalid Bedrock player map: {error}")))?;
    let mut imported = BTreeMap::new();
    let mut visited = BTreeSet::new();
    for PlayerLink { source, uuid } in links {
        if source_key(source.as_bytes()).is_none() {
            return Err(invalid(format!(
                "invalid Bedrock player record key {source:?}"
            )));
        }
        if uuid.is_nil() || imported.contains_key(&uuid) {
            return Err(invalid(format!(
                "invalid or repeated destination player UUID {uuid}"
            )));
        }
        let record_key = if source.starts_with(SERVER_PREFIX) {
            source.clone()
        } else {
            let link = load(db, &source, |field| field == "ServerId")?;
            link.get_string("ServerId")
                .filter(|key| {
                    key.starts_with(SERVER_PREFIX) && source_key(key.as_bytes()).is_some()
                })
                .ok_or_else(|| invalid(format!("invalid server record link in {source:?}")))?
                .to_owned()
        };
        if !visited.insert(record_key.clone()) {
            return Err(invalid(format!(
                "repeated Bedrock player record {record_key:?}"
            )));
        }
        visited.insert(source.clone());
        let saved = load(db, &record_key, |field| field != "internalComponents")?;
        let record = convert(&saved, uuid, terrain, items)
            .map_err(|reason| invalid(format!("player {source:?}: {reason}")))?;
        imported.insert(uuid, record);
    }
    stats.players_imported = imported.len();
    stats.other_records_excluded = stats.other_records_excluded.saturating_sub(visited.len());
    Ok(imported)
}

fn load(db: &DB, key: &str, include: impl Fn(&str) -> bool) -> Result<NbtCompound, Error> {
    let bytes = db
        .try_get(key.as_bytes())?
        .ok_or_else(|| invalid(format!("missing Bedrock player record {key:?}")))?;
    read_record(&bytes, MAX_PLAYER_BYTES, "player", include)
}

fn convert(
    saved: &NbtCompound,
    uuid: Uuid,
    terrain: &TerrainRegistry,
    items: &StorageItemRegistry,
) -> Result<NbtCompound, String> {
    if saved.get_string("identifier") != Some("minecraft:player") {
        return Err("expected a modern Bedrock player record".into());
    }
    let id = saved
        .get_int("DimensionId")
        .ok_or("missing player dimension ID")?;
    let dimension = SavedDimension::from_disk(id).map_err(|error| error.to_string())?;
    for name in ["LinksTag", "ActiveEffects", "MobEffects", "PlayerUIItems"] {
        if !list(saved, name)?.is_empty() {
            return Err(format!("the {name} state is unsupported"));
        }
    }
    if saved.get_bool("Sleeping") == Some(true) || saved.get_bool("Sitting") == Some(true) {
        return Err("sleeping or seated players are unsupported".into());
    }
    let [x, eye, z] = finite_floats(saved, "Pos").ok_or("invalid player position")?;
    let y = f64::from(eye) - f64::from(EntityType::PLAYER.eye_height);
    let limit = (CHUNK_COORDINATE_LIMIT * 16) as f32;
    if x.abs() >= limit || z.abs() >= limit || y < f64::from(i32::MIN) || y > f64::from(i32::MAX) {
        return Err("player position is outside the supported coordinate range".into());
    }
    let yaw_pitch = finite_floats::<2>(saved, "Rotation").ok_or("invalid player rotation")?;
    let mode = match saved.get_int("PlayerGameMode") {
        Some(0) => GameMode::Survival,
        Some(1) => GameMode::Creative,
        Some(2) => GameMode::Adventure,
        Some(6) => GameMode::Spectator,
        _ => return Err("unsupported Bedrock game mode".into()),
    };
    let mut record = NbtCompound::new();
    record.put_int("DataVersion", MAXIMUM_SUPPORTED_WORLD_DATA_VERSION);
    record.put_uuid("UUID", uuid);
    record.put_bool("HasPlayedBefore", true);
    record.put_string("Dimension", dimension.target().minecraft_name.to_owned());
    record.put_int("playerGameType", mode as i32);
    record.put_list(
        "Pos",
        [f64::from(x), y, f64::from(z)].map(NbtTag::Double).into(),
    );
    record.put_list("Rotation", yaw_pitch.map(NbtTag::Float).into());
    if saved.has("Motion") {
        let motion = finite_floats::<3>(saved, "Motion").ok_or("invalid player motion")?;
        record.put_list(
            "Motion",
            motion.map(|axis| NbtTag::Double(f64::from(axis))).into(),
        );
    }
    copy_experience(saved, &mut record)?;
    copy_respawn(saved, &mut record)?;
    copy_vitals(saved, &mut record)?;
    copy_items(saved, terrain, items, &mut record)?;
    Ok(record)
}

fn copy_experience(saved: &NbtCompound, record: &mut NbtCompound) -> Result<(), String> {
    let level = saved
        .get_int("PlayerLevel")
        .filter(|level| (0..=i32::MAX.isqrt()).contains(level))
        .ok_or("invalid experience level")?;
    let progress = saved
        .get_float("PlayerLevelProgress")
        .filter(|progress| (0.0..1.0).contains(progress))
        .ok_or("invalid experience progress")?;
    let points = (progress * experience::points_in_level(level) as f32).round() as i32;
    let total = experience::points_to_level(level)
        .checked_add(points)
        .filter(|total| *total < i32::MAX)
        .ok_or("experience total exceeds the supported range")?;
    record.put_int("XpLevel", level);
    record.put_float("XpP", progress);
    record.put_int("XpTotal", total);
    if let Some(seed) = saved.get_int("EnchantmentSeed") {
        record.put_int("XpSeed", seed);
    }
    Ok(())
}

fn copy_respawn(saved: &NbtCompound, record: &mut NbtCompound) -> Result<(), String> {
    let coords = ["SpawnX", "SpawnY", "SpawnZ"].map(|field| saved.get_int(field));
    match coords {
        [None, None, None] => Ok(()),
        [Some(x), Some(y), Some(z)] if [x, y, z].contains(&i32::MIN) => {
            if [x, y, z] == [i32::MIN; 3] {
                Ok(())
            } else {
                Err("incomplete player respawn coordinates".into())
            }
        }
        [Some(x), Some(y), Some(z)] => {
            let dimension = saved
                .get_int("SpawnDimension")
                .and_then(|id| SavedDimension::from_disk(id).ok())
                .ok_or("unsupported player respawn dimension")?;
            let mut respawn = NbtCompound::new();
            respawn.put_string("dimension", dimension.target().minecraft_name.to_owned());
            respawn.put("pos", NbtTag::IntArray(vec![x, y, z]));
            let from_command = saved.get_int("SpawnBlockPositionX") == Some(i32::MIN);
            respawn.put_bool("forced", from_command);
            record.put_compound("respawn", respawn);
            Ok(())
        }
        _ => Err("unsupported player respawn dimension or coordinates".into()),
    }
}

fn copy_vitals(saved: &NbtCompound, record: &mut NbtCompound) -> Result<(), String> {
    for tag in list(saved, "Attributes")? {
        let entry = tag
            .extract_compound()
            .ok_or("invalid Bedrock player attribute")?;
        let Some(name) = entry.get_string("Name") else {
            return Err("missing Bedrock player attribute name".into());
        };
        let (destination, range) = match name {
            "minecraft:health" => ("Health", 0.0..=20.0),
            "minecraft:player.hunger" => ("foodLevel", 0.0..=20.0),
            "minecraft:player.saturation" => ("foodSaturationLevel", 0.0..=20.0),
            "minecraft:player.exhaustion" => ("foodExhaustionLevel", 0.0..=20.0),
            _ => continue,
        };
        let value = entry
            .get_float("Base")
            .filter(|value| range.contains(value))
            .ok_or_else(|| format!("unsupported value for attribute {name:?}"))?;
        if record.has(destination) {
            return Err(format!("repeated player attribute {name:?}"));
        }
        if destination == "foodLevel" {
            record.put_int(destination, value as i32);
        } else {
            record.put_float(destination, value);
        }
    }
    if let Some(air) = saved.get_short("Air") {
        record.put_short("Air", air);
    }
    Ok(())
}

fn list<'a>(saved: &'a NbtCompound, name: &str) -> Result<&'a [NbtTag], String> {
    saved.get(name).map_or(Ok(&[]), |value| {
        value
            .extract_list()
            .ok_or_else(|| format!("invalid {name} list"))
    })
}

fn copy_items(
    saved: &NbtCompound,
    terrain: &TerrainRegistry,
    items: &StorageItemRegistry,
    record: &mut NbtCompound,
) -> Result<(), String> {
    let selected = saved
        .get_int("SelectedInventorySlot")
        .filter(|slot| (0..9).contains(slot))
        .ok_or("invalid selected hotbar slot")? as i8;
    let mut inventory = indexed_stacks(saved, "Inventory", 36, terrain, items)?;
    let ender = indexed_stacks(saved, "EnderChestInventory", 27, terrain, items)?;
    let armor = list(saved, "Armor")?;
    let offhand = list(saved, "Offhand")?;
    let mainhand = list(saved, "Mainhand")?;
    if armor.len() > 5 || offhand.len() > 1 || mainhand.len() > 1 {
        return Err("unsupported equipment layout".into());
    }
    for (index, entry) in armor.iter().enumerate() {
        if let Some(stack) = stack(entry, terrain, items)? {
            let slot = [103, 102, 101, 100]
                .get(index)
                .ok_or("unsupported body equipment")?;
            inventory.insert(*slot, stack);
        }
    }
    if let Some(entry) = offhand.first()
        && let Some(stack) = stack(entry, terrain, items)?
    {
        inventory.insert(-106, stack);
    }
    if let Some(entry) = mainhand.first() {
        match (stack(entry, terrain, items)?, inventory.get(&selected)) {
            (None, None) => {}
            (Some(held), Some(slot))
                if held.item_count == slot.item_count
                    && held.are_items_and_components_equal(slot) => {}
            _ => return Err("main hand differs from the selected hotbar slot".into()),
        }
    }
    record.put_int("SelectedItemSlot", i32::from(selected));
    record.put_list("Inventory", encode_stacks(inventory));
    record.put_list("EnderItems", encode_stacks(ender));
    Ok(())
}

fn indexed_stacks(
    saved: &NbtCompound,
    name: &str,
    size: i8,
    terrain: &TerrainRegistry,
    items: &StorageItemRegistry,
) -> Result<BTreeMap<i8, ItemStack>, String> {
    let mut inventory = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for entry in list(saved, name)? {
        let slot = entry
            .extract_compound()
            .and_then(|item| item.get_byte("Slot"))
            .filter(|slot| (0..size).contains(slot))
            .ok_or_else(|| format!("invalid slot in {name}"))?;
        if !seen.insert(slot) {
            return Err(format!("repeated slot {slot} in {name}"));
        }
        if let Some(stack) = stack(entry, terrain, items)
            .map_err(|reason| format!("{name} slot {slot}: {reason}"))?
        {
            inventory.insert(slot, stack);
        }
    }
    Ok(inventory)
}

fn stack(
    entry: &NbtTag,
    terrain: &TerrainRegistry,
    items: &StorageItemRegistry,
) -> Result<Option<ItemStack>, String> {
    let entry = entry
        .extract_compound()
        .ok_or("invalid Bedrock item compound")?;
    if entry.is_empty()
        || entry.get_byte("Count") == Some(0) && entry.get_string("Name") == Some("")
    {
        return Ok(None);
    }
    if let Some(tag) = entry.get_compound("tag")
        && let Some(field) = tag.get("Damage")
    {
        let damage = field.extract_int().ok_or("invalid item durability")?;
        let mut entry = entry.clone();
        let mut tag = tag.clone();
        tag.child_tags.remove("Damage");
        entry.put_compound("tag", tag);
        let mut item = convert_stack(&entry, terrain, items)?;
        let limit = item
            .get_max_damage()
            .ok_or("durability on an item without a damage component")?;
        if !(0..limit).contains(&damage) {
            return Err("invalid item durability".into());
        }
        if damage != 0 {
            item.set_data_component(DamageImpl { damage });
        }
        return Ok(Some(item));
    }
    convert_stack(entry, terrain, items).map(Some)
}

fn encode_stacks(items: BTreeMap<i8, ItemStack>) -> Vec<NbtTag> {
    items
        .into_iter()
        .map(|(slot, stack)| {
            let mut record = NbtCompound::new();
            record.put_byte("Slot", slot);
            stack.write_item_stack(&mut record);
            NbtTag::Compound(record)
        })
        .collect()
}
