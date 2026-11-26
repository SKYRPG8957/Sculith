use pumpkin_nbt::tag::NbtTag;
use sculith_util::math::vector2::Vector2;

use crate::world_info::ticking_areas::{
    AreaShape, MAX_AREAS, TickingArea, TickingAreaDimensions, same_name, valid_name,
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
    use pumpkin_nbt::{Nbt, compound::NbtCompound};

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
}
