use std::{collections::BTreeSet, io::Cursor};

use leveldb::DB;
use pumpkin_data::{
    attributes::Attributes, chicken_sound_variant::ChickenSoundVariant,
    cow_sound_variant::CowSoundVariant, entity::EntityType, pig_sound_variant::PigSoundVariant,
};
use pumpkin_nbt::{
    COMPOUND_ID, END_ID,
    compound::NbtCompound,
    deserializer::{NbtReadHelper, NbtReadHelperBedrockDisk},
    tag::NbtTag,
};
use sculith_util::math::{position::BlockPos, vector2::Vector2};
use uuid::Uuid;

use super::{DIAGNOSTIC_LIMIT, Error, ImportStats, OVERWORLD, SkippedActor, invalid};

pub(super) const ACTOR_PREFIX: &[u8; 11] = b"actorprefix";
const ACTOR_INDEX_PREFIX: &[u8; 4] = b"digp";
const MAX_ACTOR_BYTES: usize = 64 * 1024;
const MAX_ACTORS_PER_CHUNK: usize = 4096;
// Java's modified UTF-8 can take twice as many bytes as the source UTF-8.
const MAX_ACTOR_TEXT_BYTES: usize = u16::MAX as usize / 2;
type ConversionError = &'static str;

pub(super) fn read_chunk(
    db: &DB,
    chunk: Vector2<i32>,
    seed: i64,
    riders: &BTreeSet<i64>,
    stats: &mut ImportStats,
) -> Result<Vec<NbtCompound>, Error> {
    let mut converted = Vec::new();
    if stats.entity_records_excluded == 0 {
        return Ok(converted);
    }
    let mut index_key = [0; 12];
    index_key[..4].copy_from_slice(ACTOR_INDEX_PREFIX);
    index_key[4..8].copy_from_slice(&chunk.x.to_le_bytes());
    index_key[8..].copy_from_slice(&chunk.y.to_le_bytes());
    let Some(index) = db.try_get(&index_key)? else {
        return Ok(converted);
    };
    let (keys, remainder) = index.as_chunks::<8>();
    if !remainder.is_empty() || keys.len() > MAX_ACTORS_PER_CHUNK {
        skip(stats, chunk, "<actor index>", "invalid chunk actor index");
        return Ok(converted);
    }
    let mut entity_key = [0; 19];
    entity_key[..ACTOR_PREFIX.len()].copy_from_slice(ACTOR_PREFIX);
    let mut seen = BTreeSet::new();
    for key in keys.iter().filter(|key| seen.insert(**key)) {
        entity_key[ACTOR_PREFIX.len()..].copy_from_slice(key);
        let Some(bytes) = db.try_get(&entity_key)? else {
            skip(
                stats,
                chunk,
                "<missing actor>",
                "actor index points to an absent record",
            );
            continue;
        };
        // The official writer stores the raw storage key as a string in this component.
        let saved = match read_record(&bytes, |field| field != "internalComponents") {
            Ok(saved) => saved,
            Err(error) => {
                skip(stats, chunk, "<actor record>", error.to_string());
                continue;
            }
        };
        let kind = saved
            .get_string("identifier")
            .unwrap_or("<missing identifier>");
        match convert(&saved, kind, chunk, seed, *key, riders) {
            Ok(record) => {
                converted.push(record);
                stats.entities_imported += 1;
                stats.entity_records_excluded -= 1;
            }
            Err(reason) => skip(stats, chunk, kind, reason),
        }
    }
    Ok(converted)
}

fn skip(stats: &mut ImportStats, chunk: Vector2<i32>, kind: &str, reason: impl Into<String>) {
    if stats.skipped_actor_examples.len() < DIAGNOSTIC_LIMIT {
        stats.skipped_actor_examples.push(SkippedActor {
            kind: kind.to_owned(),
            chunk,
            reason: reason.into(),
        });
    }
}

pub(super) fn collect_riders(bytes: &[u8], riders: &mut BTreeSet<i64>) -> Result<(), Error> {
    // Bedrock saves the passenger ID on the vehicle, even across actor types.
    const LINK_FIELD: &[u8] = b"\x08\0LinksTag";
    if !bytes
        .windows(LINK_FIELD.len())
        .any(|window| window == LINK_FIELD)
    {
        return Ok(());
    }
    let record = read_record(bytes, |field| field == "LinksTag")?;
    if let Some(links) = record.get("LinksTag") {
        let links = links
            .extract_list()
            .ok_or_else(|| invalid("invalid Bedrock actor links"))?;
        for link in links {
            let id = link
                .extract_compound()
                .and_then(|tag| tag.get_long("entityID"))
                .ok_or_else(|| invalid("invalid Bedrock actor passenger ID"))?;
            riders.insert(id);
        }
    }
    Ok(())
}

fn read_record(bytes: &[u8], include: impl Fn(&str) -> bool) -> Result<NbtCompound, Error> {
    if bytes.len() > MAX_ACTOR_BYTES {
        return Err(invalid("oversized Bedrock actor record"));
    }
    let mut reader = NbtReadHelperBedrockDisk::new(Cursor::new(bytes));
    if reader.get_u8()? != COMPOUND_ID {
        return Err(invalid("expected a Bedrock actor compound"));
    }
    reader.skip_string()?;
    let mut saved = NbtCompound::new();
    loop {
        let tag = reader.get_u8()?;
        if tag == END_ID {
            break;
        }
        let name = reader.get_string()?;
        if include(&name) {
            let value = NbtTag::deserialize_data(&mut reader, tag)?;
            if saved
                .child_tags
                .insert(name.as_ref().into(), value)
                .is_some()
            {
                return Err(invalid("duplicate Bedrock actor field"));
            }
        } else {
            NbtTag::skip_data(&mut reader, tag)?;
        }
    }
    if reader.reader().position() != bytes.len() as u64 {
        return Err(invalid("trailing bytes after Bedrock actor compound"));
    }
    Ok(saved)
}

fn floats<const N: usize>(saved: &NbtCompound, field: &str) -> Option<[f32; N]> {
    let entries: &[NbtTag; N] = saved.get_list(field)?.try_into().ok()?;
    let mut values = [0.; N];
    for (value, tag) in values.iter_mut().zip(entries) {
        *value = tag.extract_float().filter(|coord| coord.is_finite())?;
    }
    Some(values)
}

fn convert(
    saved: &NbtCompound,
    kind: &str,
    chunk: Vector2<i32>,
    seed: i64,
    key: [u8; 8],
    riders: &BTreeSet<i64>,
) -> Result<NbtCompound, ConversionError> {
    let entity = match kind {
        "minecraft:pig" => &EntityType::PIG,
        "minecraft:cow" => &EntityType::COW,
        "minecraft:sheep" => &EntityType::SHEEP,
        "minecraft:chicken" => &EntityType::CHICKEN,
        _ => return Err("unsupported actor type"),
    };
    let unique_id = saved
        .get_long("UniqueID")
        .ok_or("missing actor unique ID")?;
    if riders.contains(&unique_id) {
        return Err("unsupported actor riding relationship");
    }
    validate_unlinked(saved)?;
    let [x, y, z] = floats(saved, "Pos").ok_or("invalid actor position")?;
    if BlockPos::new(x.floor() as i32, y.floor() as i32, z.floor() as i32).chunk_position() != chunk
        || !(OVERWORLD.min_y as f32..(OVERWORLD.min_y + OVERWORLD.height) as f32).contains(&y)
    {
        return Err("actor position is outside its indexed chunk");
    }
    let rotation = floats::<2>(saved, "Rotation").ok_or("invalid actor rotation")?;
    let mut record = NbtCompound::new();
    record.put_string("id", kind.to_owned());
    let identity = [
        b"sculith:bedrock-actor:".as_slice(),
        &seed.to_le_bytes(),
        &key,
    ]
    .concat();
    record.put_uuid("UUID", Uuid::new_v3(&Uuid::NAMESPACE_OID, &identity));
    record.put_list("Pos", [x, y, z].map(|axis| f64::from(axis).into()).into());
    record.put_list("Rotation", rotation.map(NbtTag::Float).into());
    if saved.has("Motion") {
        let motion = floats::<3>(saved, "Motion").ok_or("invalid actor motion")?;
        record.put_list("Motion", motion.map(|axis| f64::from(axis).into()).into());
    }
    if let Some(name) = saved
        .get_string("CustomName")
        .filter(|name| !name.is_empty())
    {
        if name.len() > MAX_ACTOR_TEXT_BYTES {
            return Err("actor name is too long");
        }
        let mut literal = NbtCompound::new();
        literal.put_string("text", name.to_owned());
        record.put_compound("CustomName", literal);
    }
    if let Some(tags) = saved.get_list("Tags") {
        if tags.iter().any(|tag| {
            tag.extract_string()
                .is_none_or(|name| name.len() > MAX_ACTOR_TEXT_BYTES)
        }) {
            return Err("invalid actor tags");
        }
        record.put_list("Tags", tags.to_vec());
    }
    for (before, after) in [
        ("Persistent", "PersistenceRequired"),
        ("OnGround", "OnGround"),
        ("canPickupItems", "CanPickUpLoot"),
        ("CustomNameVisible", "CustomNameVisible"),
        ("GrowthPaused", "AgeLocked"),
        ("NoAI", "NoAI"),
        ("Invulnerable", "Invulnerable"),
        ("Silent", "Silent"),
    ] {
        if let Some(value) = saved.get_bool(before) {
            record.put_bool(after, value);
        }
    }
    for tag in ["Air", "Fire"] {
        if let Some(value) = saved.get_short(tag) {
            record.put_short(tag, value);
        }
    }
    let age = age(saved)?;
    record.put_int("Age", age);
    attributes(saved, &mut record)?;
    variant(saved, entity, age, &mut record)?;
    Ok(record)
}

fn validate_unlinked(saved: &NbtCompound) -> Result<(), ConversionError> {
    if ["LeasherID", "OwnerNew", "OwnerID", "TargetID", "LoveCause"]
        .into_iter()
        .any(|key| saved.has(key) && !matches!(saved.get_long(key), Some(-1 | 0)))
        || ["LinksTag", "Passengers"]
            .into_iter()
            .any(|key| saved.has(key) && !saved.get_list(key).is_some_and(<[NbtTag]>::is_empty))
        || saved.get_bool("Sitting").unwrap_or(false)
        || saved.get_int("InLove").unwrap_or(0) != 0
    {
        return Err("unsupported actor relationship");
    }
    if ["ActiveEffects", "active_effects"]
        .into_iter()
        .any(|key| saved.has(key) && !saved.get_list(key).is_some_and(<[NbtTag]>::is_empty))
    {
        return Err("unsupported actor status effects");
    }
    if ["Armor", "Mainhand", "Offhand", "Items"]
        .into_iter()
        .any(|key| {
            saved.has(key)
                && !saved.get_list(key).is_some_and(|items| {
                    items
                        .iter()
                        .all(|item| item.extract_compound().is_some_and(NbtCompound::is_empty))
                })
        })
        || ["Saddled", "Saddle"]
            .into_iter()
            .any(|key| saved.get_bool(key).unwrap_or(false))
        || saved.get_list("definitions").is_some_and(|groups| {
            groups
                .iter()
                .any(|group| group.extract_string() == Some("+minecraft:pig_saddled"))
        })
    {
        return Err("unsupported actor equipment");
    }
    Ok(())
}

fn age(saved: &NbtCompound) -> Result<i32, ConversionError> {
    let age = saved.get_int("Age").unwrap_or(0);
    let cooldown = saved.get_int("BreedCooldown").unwrap_or(0);
    if cooldown < 0
        || age != 0 && cooldown != 0
        || saved
            .get_bool("IsBaby")
            .is_some_and(|baby| baby != (age < 0))
    {
        return Err("inconsistent actor age or breeding cooldown");
    }
    Ok(if cooldown == 0 { age } else { cooldown })
}

fn attributes(saved: &NbtCompound, record: &mut NbtCompound) -> Result<(), ConversionError> {
    let mut seen_health = false;
    let mut seen_speed = false;
    let mut packed = Vec::with_capacity(2);
    for tag in saved
        .get_list("Attributes")
        .ok_or("missing actor attributes")?
    {
        let value = tag.extract_compound().ok_or("invalid actor attribute")?;
        let (attribute, seen) = match value.get_string("Name") {
            Some("minecraft:health") => (&Attributes::MAX_HEALTH, &mut seen_health),
            Some("minecraft:movement") => (&Attributes::MOVEMENT_SPEED, &mut seen_speed),
            _ => return Err("unsupported actor attribute"),
        };
        if *seen
            || value.has("Modifiers")
                && !value
                    .get_list("Modifiers")
                    .is_some_and(<[NbtTag]>::is_empty)
        {
            return Err("unsupported actor attribute modifiers or duplicate");
        }
        *seen = true;
        let base = value
            .get_float("Base")
            .ok_or("missing actor attribute value")?;
        if !(attribute.min_value..=attribute.max_value).contains(&f64::from(base)) {
            return Err("invalid actor attribute base value");
        }
        if attribute.id == Attributes::MAX_HEALTH.id {
            let health = value.get_float("Current").unwrap_or(base);
            if !(0.0 < health && health <= base) {
                return Err("invalid actor health");
            }
            record.put_float("Health", health);
        }
        let mut field = NbtCompound::new();
        field.put_string("id", attribute.name.to_owned());
        field.put_double("base", f64::from(base));
        packed.push(NbtTag::Compound(field));
    }
    if !seen_health {
        return Err("missing actor health");
    }
    record.put_list("attributes", packed);
    Ok(())
}

fn variant(
    saved: &NbtCompound,
    entity: &EntityType,
    age: i32,
    record: &mut NbtCompound,
) -> Result<(), ConversionError> {
    if entity == &EntityType::SHEEP {
        let color = saved.get_byte("Color").unwrap_or(0);
        if !(0..16).contains(&color) {
            return Err("unsupported sheep color");
        }
        record.put_byte("Color", color);
        record.put_bool("Sheared", saved.get_bool("Sheared").unwrap_or(false));
        return Ok(());
    }

    let properties = saved.get_compound("properties");
    let climate = properties
        .and_then(|props| props.get_string("minecraft:climate_variant"))
        .unwrap_or("temperate");
    let egg_slot = match climate {
        "temperate" => 0,
        "warm" => 1,
        "cold" => 2,
        _ => return Err("unsupported actor climate variant"),
    };
    record.put_string("variant", format!("minecraft:{climate}"));
    let sound = properties
        .and_then(|props| props.get_string("minecraft:sound_variant"))
        .unwrap_or("default");
    let sound = sound.strip_prefix("minecraft:").unwrap_or(sound);
    let sound = if sound == "default" { "classic" } else { sound };
    let valid_sound = if entity == &EntityType::PIG {
        PigSoundVariant::from_name(sound).is_some()
    } else if entity == &EntityType::COW {
        CowSoundVariant::from_name(sound).is_some()
    } else {
        ChickenSoundVariant::from_name(sound).is_some()
    };
    if !valid_sound {
        return Err("unsupported actor sound variant");
    }
    record.put_string("sound_variant", format!("minecraft:{sound}"));
    if entity == &EntityType::CHICKEN && age >= 0 {
        let entries = saved
            .get_list("entries")
            .ok_or("missing chicken egg timer")?;
        if entries.len() != 3 {
            return Err("unsupported chicken egg timers");
        }
        let timer = entries[egg_slot]
            .extract_compound()
            .and_then(|entry| {
                (!entry.get_bool("StopSpawning").unwrap_or(false))
                    .then(|| entry.get_int("SpawnTimer"))
                    .flatten()
            })
            .filter(|&timer| timer >= 0)
            .ok_or("unsupported chicken egg timer")?;
        record.put_int("EggLayTime", timer);
    }
    Ok(())
}
