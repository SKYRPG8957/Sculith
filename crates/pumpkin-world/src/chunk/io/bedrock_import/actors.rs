use std::collections::{BTreeMap, BTreeSet};

use leveldb::DB;
use pumpkin_data::{
    attributes::Attributes, chicken_sound_variant::ChickenSoundVariant,
    cow_sound_variant::CowSoundVariant, entity::EntityType, pig_sound_variant::PigSoundVariant,
};
use pumpkin_nbt::{compound::NbtCompound, tag::NbtTag};
use sculith_util::math::{position::BlockPos, vector2::Vector2};
use uuid::Uuid;

use super::{
    DIAGNOSTIC_LIMIT, Error, ImportStats, SavedDimension, SkippedActor, finite_floats, invalid,
    read_record,
};

pub(super) const ACTOR_PREFIX: &[u8; 11] = b"actorprefix";
const ACTOR_INDEX_PREFIX: &[u8; 4] = b"digp";
const MAX_ACTOR_BYTES: usize = 64 * 1024;
const MAX_ACTORS_PER_CHUNK: usize = 4096;
// Java's modified UTF-8 can take twice as many bytes as the source UTF-8.
const MAX_ACTOR_TEXT_BYTES: usize = u16::MAX as usize / 2;
type ConversionError = &'static str;

#[derive(Default)]
pub(super) struct References {
    riders: BTreeSet<i64>,
    dimensions: BTreeMap<([u8; 8], [i32; 2]), Option<i32>>,
}

impl References {
    pub(super) fn observe(&mut self, key: &[u8], value: &[u8]) -> Result<(), Error> {
        if key.starts_with(ACTOR_PREFIX) {
            return collect_riders(value, &mut self.riders);
        }
        let Some((coordinates, dimension)) = key
            .strip_prefix(ACTOR_INDEX_PREFIX)
            .and_then(<[u8]>::split_first_chunk::<8>)
        else {
            return Ok(());
        };
        let dimension = match *dimension {
            [] => 0,
            [a, b, c, d] => i32::from_le_bytes([a, b, c, d]),
            _ => return Ok(()),
        };
        let (actors, remainder) = value.as_chunks::<8>();
        if remainder.is_empty() && actors.len() <= MAX_ACTORS_PER_CHUNK {
            let [x0, x1, x2, x3, z0, z1, z2, z3] = *coordinates;
            let column = [
                i32::from_le_bytes([x0, x1, x2, x3]),
                i32::from_le_bytes([z0, z1, z2, z3]),
            ];
            // The saved position resolves stale references in other columns.
            // Matching horizontal coordinates cannot resolve two dimensions.
            for &actor in actors {
                self.dimensions
                    .entry((actor, column))
                    .and_modify(|known| {
                        if *known != Some(dimension) {
                            *known = None;
                        }
                    })
                    .or_insert(Some(dimension));
            }
        }
        Ok(())
    }
}

pub(super) fn read_chunk(
    db: &DB,
    dimension: SavedDimension,
    chunk: Vector2<i32>,
    seed: i64,
    references: &References,
    stats: &mut ImportStats,
) -> Result<Vec<NbtCompound>, Error> {
    let mut converted = Vec::new();
    if stats.entity_records_excluded == 0 {
        return Ok(converted);
    }
    let mut index_key = [0; 16];
    index_key[..4].copy_from_slice(ACTOR_INDEX_PREFIX);
    let size = 4 + dimension.write_column_prefix(chunk, &mut index_key[4..]);
    let Some(index) = db.try_get(&index_key[..size])? else {
        return Ok(converted);
    };
    let (keys, remainder) = index.as_chunks::<8>();
    if !remainder.is_empty() || keys.len() > MAX_ACTORS_PER_CHUNK {
        skip(
            stats,
            dimension,
            chunk,
            "<actor index>",
            "invalid chunk actor index",
        );
        return Ok(converted);
    }
    let mut entity_key = [0; 19];
    entity_key[..ACTOR_PREFIX.len()].copy_from_slice(ACTOR_PREFIX);
    let mut seen = BTreeSet::new();
    for key in keys.iter().filter(|key| seen.insert(**key)) {
        if references.dimensions.get(&(*key, [chunk.x, chunk.y])) == Some(&None) {
            skip(
                stats,
                dimension,
                chunk,
                "<actor index>",
                "actor is indexed at the same coordinates in multiple dimensions",
            );
            continue;
        }
        entity_key[ACTOR_PREFIX.len()..].copy_from_slice(key);
        let Some(bytes) = db.try_get(&entity_key)? else {
            skip(
                stats,
                dimension,
                chunk,
                "<missing actor>",
                "actor index points to an absent record",
            );
            continue;
        };
        // The official writer stores the raw storage key as a string in this component.
        let saved = match read_record(&bytes, MAX_ACTOR_BYTES, "actor", |field| {
            field != "internalComponents"
        }) {
            Ok(saved) => saved,
            Err(error) => {
                skip(stats, dimension, chunk, "<actor record>", error.to_string());
                continue;
            }
        };
        let kind = saved
            .get_string("identifier")
            .unwrap_or("<missing identifier>");
        match convert(
            &saved,
            kind,
            dimension,
            chunk,
            seed,
            *key,
            &references.riders,
        ) {
            Ok(record) => {
                converted.push(record);
                stats.entities_imported += 1;
                stats.entity_records_excluded -= 1;
            }
            Err(reason) => skip(stats, dimension, chunk, kind, reason),
        }
    }
    Ok(converted)
}

fn skip(
    stats: &mut ImportStats,
    dimension: SavedDimension,
    chunk: Vector2<i32>,
    kind: &str,
    reason: impl Into<String>,
) {
    if stats.skipped_actor_examples.len() < DIAGNOSTIC_LIMIT {
        stats.skipped_actor_examples.push(SkippedActor {
            kind: kind.to_owned(),
            chunk,
            reason: format!("{}: {}", dimension.target().minecraft_name, reason.into()),
        });
    }
}

fn collect_riders(bytes: &[u8], riders: &mut BTreeSet<i64>) -> Result<(), Error> {
    // Bedrock saves the passenger ID on the vehicle, even across actor types.
    const LINK_FIELD: &[u8] = b"\x08\0LinksTag";
    if !bytes
        .windows(LINK_FIELD.len())
        .any(|window| window == LINK_FIELD)
    {
        return Ok(());
    }
    let record = read_record(bytes, MAX_ACTOR_BYTES, "actor", |field| field == "LinksTag")?;
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

fn convert(
    saved: &NbtCompound,
    kind: &str,
    dimension: SavedDimension,
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
    let [x, y, z] = finite_floats(saved, "Pos").ok_or("invalid actor position")?;
    let min_y = dimension.target().min_y;
    let max_y = min_y + 16 * dimension.disk_sections() as i32;
    if BlockPos::new(x.floor() as i32, y.floor() as i32, z.floor() as i32).chunk_position() != chunk
        || !(min_y as f32..max_y as f32).contains(&y)
    {
        return Err("actor position is outside its indexed chunk");
    }
    let rotation = finite_floats::<2>(saved, "Rotation").ok_or("invalid actor rotation")?;
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
        let motion = finite_floats::<3>(saved, "Motion").ok_or("invalid actor motion")?;
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

#[cfg(test)]
mod reference_tests {
    use super::*;

    #[test]
    fn actor_index_checks_for_ambiguity_at_the_same_coordinates() {
        let mut references = References::default();
        let pos = Vector2::new(0, 0);
        let mut index = [0; 16];
        index[..4].copy_from_slice(ACTOR_INDEX_PREFIX);
        let actor = [4; 8];
        let overworld = 4 + SavedDimension::Overworld.write_column_prefix(pos, &mut index[4..]);
        references
            .observe(&index[..overworld], &[actor, actor].concat())
            .unwrap();
        let origin = (actor, [0, 0]);
        assert_eq!(references.dimensions[&origin], Some(0));
        let other =
            4 + SavedDimension::Nether.write_column_prefix(Vector2::new(1, -1), &mut index[4..]);
        references.observe(&index[..other], &actor).unwrap();
        assert_eq!(references.dimensions[&origin], Some(0));
        assert_eq!(references.dimensions[&(actor, [1, -1])], Some(1));
        let nether = 4 + SavedDimension::Nether.write_column_prefix(pos, &mut index[4..]);
        references.observe(&index[..nether], &actor).unwrap();
        assert!(references.dimensions[&origin].is_none());
    }
}
