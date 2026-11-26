use std::{collections::BTreeMap, path::Path};

use leveldb::{DB, WriteBatch};
use pumpkin_nbt::{Nbt, compound::NbtCompound, tag::NbtTag};
use sculith_util::math::vector2::Vector2;
use uuid::Uuid;

use crate::world_info::ticking_areas::{
    self as area_metadata, AreaShape, MAX_AREAS, TickingArea, TickingAreaDimensions, same_name,
    valid_name,
};

use super::{Error, SavedDimension, invalid, read_record};

pub(super) const PREFIX: &[u8] = b"tickingarea_";
const MAX_RECORD_BYTES: usize = 65_536;

pub(super) fn add_record(data: &[u8], areas: &mut TickingAreaDimensions) -> Result<(), Error> {
    let (dimension, area) = decode(data)?;
    if areas.values().map(Vec::len).sum::<usize>() >= MAX_AREAS {
        return Err(invalid("too many Bedrock ticking areas"));
    }
    let local = areas
        .entry(dimension.target().minecraft_name.to_owned())
        .or_default();
    if local.iter().any(|saved| same_name(&saved.name, &area.name)) {
        return Err(invalid(
            "duplicate ticking area name in the Bedrock dimension",
        ));
    }
    local.push(area);
    Ok(())
}

pub(super) fn export(db: &DB, sculith: &Path) -> Result<usize, Error> {
    let areas = area_metadata::read(sculith)?;
    let mut desired = BTreeMap::new();
    for (dimension, areas) in &areas {
        if areas.is_empty() {
            continue;
        }
        let dimension = [
            SavedDimension::Overworld,
            SavedDimension::Nether,
            SavedDimension::End,
        ]
        .into_iter()
        .find(|target| target.target().minecraft_name == dimension)
        .ok_or_else(|| invalid(format!("unsupported ticking area dimension {dimension}")))?;
        desired.extend(
            areas
                .iter()
                .map(|area| ((dimension, area.name.to_lowercase()), area)),
        );
    }

    let exported = desired.len();
    let mut batch = WriteBatch::new();
    let mut records = db.new_iter()?;
    records.seek(PREFIX);
    while let Some((key, value)) = records
        .key()
        .filter(|key| key.starts_with(PREFIX))
        .zip(records.value())
    {
        let (dimension, saved) = decode(value)?;
        match desired.remove(&(dimension, saved.name.to_lowercase())) {
            Some(area) if *area != saved => batch.put(key, &encode(dimension, area)?),
            Some(_) => {}
            None => batch.delete(key),
        }
        records.next();
    }
    records.status()?;
    drop(records);

    for ((dimension, _), area) in desired {
        let key = format!("tickingarea_{}", Uuid::new_v4());
        batch.put(key.as_bytes(), &encode(dimension, area)?);
    }
    if batch.count() != 0 {
        db.write(batch, true)?;
    }
    Ok(exported)
}

fn encode(dimension: SavedDimension, area: &TickingArea) -> Result<Vec<u8>, Error> {
    let (min, max, circle) = match area.shape {
        AreaShape::Rectangle { min, max } => (min, max, false),
        AreaShape::Circle { center, radius } => (
            center.map(|coordinate| coordinate - radius),
            center.map(|coordinate| coordinate + radius),
            true,
        ),
    };
    let mut compound = NbtCompound::new();
    compound.put_int("Dimension", dimension as i32);
    compound.put_bool("IsCircle", circle);
    compound.put_int("MinX", min[0] << 4);
    compound.put_int("MinZ", min[1] << 4);
    compound.put_int("MaxX", max[0] << 4);
    compound.put_int("MaxZ", max[1] << 4);
    compound.put_string("Name", area.name.clone());
    if area.preload {
        compound.put_bool("Preload", true);
    }
    let mut bytes = Vec::new();
    Nbt::from(compound).write_to_writer_bedrock_disk(&mut bytes)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(invalid("oversized Bedrock ticking area record"));
    }
    Ok(bytes)
}

fn decode(data: &[u8]) -> Result<(SavedDimension, TickingArea), Error> {
    let nbt = read_record(data, MAX_RECORD_BYTES, "ticking area", |key| {
        matches!(
            key,
            "Dimension" | "IsCircle" | "MinX" | "MinZ" | "MaxX" | "MaxZ" | "Name" | "Preload"
        )
    })?;
    let dimension = nbt
        .get_int("Dimension")
        .ok_or_else(|| invalid("missing Bedrock ticking area dimension"))?;
    let dimension = SavedDimension::from_disk(dimension)?;
    let name = nbt
        .get_string("Name")
        .filter(|name| valid_name(name))
        .ok_or_else(|| invalid("invalid Bedrock ticking area name"))?;
    let coordinate = |name: &str| {
        nbt.get_int(name)
            .filter(|value| value.rem_euclid(16) == 0)
            .map(|value| value >> 4)
            .ok_or_else(|| {
                invalid(format!(
                    "expected chunk-aligned Bedrock ticking area {name}"
                ))
            })
    };
    let min = Vector2::new(coordinate("MinX")?, coordinate("MinZ")?);
    let max = Vector2::new(coordinate("MaxX")?, coordinate("MaxZ")?);
    if min.x > max.x || min.y > max.y {
        return Err(invalid("reversed Bedrock ticking area bounds"));
    }
    let shape = match nbt.get_byte("IsCircle") {
        Some(0) => AreaShape::rectangle(min, max),
        Some(1) => {
            // Native circular areas store the bounding columns of a square footprint.
            let width = max.x - min.x;
            if width != max.y - min.y || width % 2 != 0 {
                return Err(invalid("invalid Bedrock circular ticking area bounds"));
            }
            let radius = width / 2;
            AreaShape::circle(min.add_raw(radius, radius), radius)
        }
        _ => return Err(invalid("invalid Bedrock ticking area shape field")),
    };
    shape.validate().map_err(invalid)?;
    let preload = match nbt.child_tags.get("Preload") {
        None | Some(NbtTag::Byte(0)) => false,
        Some(NbtTag::Byte(1)) => true,
        _ => return Err(invalid("invalid Bedrock ticking area preload field")),
    };
    Ok((
        dimension,
        TickingArea {
            name: name.to_owned(),
            shape,
            preload,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(dimension: i32, max_x: i32, preload: i8) -> Vec<u8> {
        let mut compound = NbtCompound::new();
        compound.put_int("Dimension", dimension);
        compound.put_int("MinX", 976);
        compound.put_int("MinZ", 976);
        compound.put_int("MaxX", max_x);
        compound.put_int("MaxZ", 1008);
        compound.put_byte("IsCircle", 1);
        compound.put_byte("Preload", preload);
        compound.put_string("Name", "Circle".to_owned());
        let mut bytes = Vec::new();
        Nbt::from(compound)
            .write_to_writer_bedrock_disk(&mut bytes)
            .unwrap();
        bytes
    }

    #[test]
    fn reads_circular_area_flags_and_dimension_from_bedrock_nbt() {
        let mut areas = TickingAreaDimensions::new();
        add_record(&record(0, 1008, 1), &mut areas).unwrap();
        add_record(&record(1, 1008, 0), &mut areas).unwrap();
        let overworld = &areas["minecraft:overworld"][0];
        assert_eq!(overworld.shape, AreaShape::circle(Vector2::new(62, 62), 1));
        assert_eq!(overworld.name, "Circle");
        assert!(overworld.preload);
        assert!(!areas["minecraft:the_nether"][0].preload);
        assert!(add_record(&record(0, 1008, 0), &mut areas).is_err());
        assert_eq!(areas["minecraft:overworld"].len(), 1);
    }

    #[test]
    fn rejects_unaligned_or_invalid_bedrock_area_metadata() {
        assert!(decode(&record(0, 1009, 1)).is_err());
        assert!(decode(&record(0, 1008, 2)).is_err());
        assert!(decode(&record(3, 1008, 1)).is_err());
        assert!(decode(&record(0, 1024, 1)).is_err());
    }

    #[test]
    fn exports_ticking_areas_with_stable_ids_and_preserves_other_records() {
        let sculith = tempfile::tempdir().unwrap();
        let db = DB::open("native-areas", leveldb::in_memory()).unwrap();
        let (_, mut edited) = decode(&record(0, 1008, 1)).unwrap();
        edited.preload = false;
        let added = TickingArea {
            name: "Inside Nether".to_owned(),
            shape: AreaShape::rectangle(Vector2::new(-2, -3), Vector2::new(0, -2)),
            preload: true,
        };
        let saved = TickingAreaDimensions::from([
            ("minecraft:overworld".to_owned(), vec![edited.clone()]),
            ("minecraft:the_nether".to_owned(), vec![added]),
        ]);
        area_metadata::write(sculith.path(), &saved).unwrap();
        db.put(b"other", b"preserved").unwrap();
        db.put(b"tickingarea_original", &record(0, 1008, 1))
            .unwrap();
        db.put(b"tickingarea_removed", &record(2, 1008, 0)).unwrap();

        assert_eq!(export(&db, sculith.path()).unwrap(), 2);
        let after = db
            .new_iter()
            .unwrap()
            .entries()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(after.len(), 3);
        assert_eq!(db.try_get(b"other").unwrap().unwrap(), b"preserved");
        assert!(db.try_get(b"tickingarea_removed").unwrap().is_none());
        let encoded = db.try_get(b"tickingarea_original").unwrap().unwrap();
        assert_eq!(decode(&encoded).unwrap().1, edited);
        let mut output = TickingAreaDimensions::new();
        for (key, bytes) in &after {
            if key.starts_with(PREFIX) {
                add_record(bytes, &mut output).unwrap();
            }
        }
        assert_eq!(output, saved);
        assert_eq!(export(&db, sculith.path()).unwrap(), 2);
        assert_eq!(
            db.new_iter()
                .unwrap()
                .entries()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            after
        );
    }
}
