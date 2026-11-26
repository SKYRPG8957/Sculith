//! On-disk ticking areas shared by the engine and the Bedrock importer.

use std::{collections::BTreeMap, fs, io, path::Path};

use rustc_hash::FxHashSet;
use sculith_util::math::vector2::Vector2;
use serde::{Deserialize, Serialize};

use crate::chunk::io::write_file_atomically;

pub const MAX_AREAS: usize = 10;
const MAX_RECTANGLE_COLUMNS: i64 = 100;
const MAX_RADIUS: i32 = 4;
const MIN_COLUMN: i32 = -30_000_000 >> 4;
const MAX_COLUMN: i32 = 29_999_999 >> 4;
const FILE_NAME: &str = "sculith-ticking-areas.json";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AreaShape {
    Rectangle { min: [i32; 2], max: [i32; 2] },
    Circle { center: [i32; 2], radius: i32 },
}

impl AreaShape {
    #[must_use]
    pub fn rectangle(from: Vector2<i32>, to: Vector2<i32>) -> Self {
        Self::Rectangle {
            min: [from.x.min(to.x), from.y.min(to.y)],
            max: [from.x.max(to.x), from.y.max(to.y)],
        }
    }

    #[must_use]
    pub const fn circle(center: Vector2<i32>, radius: i32) -> Self {
        Self::Circle {
            center: [center.x, center.y],
            radius,
        }
    }

    /// Checks the bounds and the number of columns before tickets are created.
    pub fn validate(self) -> Result<(), &'static str> {
        match self {
            Self::Circle { center, radius } => {
                if !(0..=MAX_RADIUS).contains(&radius) {
                    return Err("Ticking area radius must be between 0 and 4 chunks");
                }
                if !center.iter().all(|&pos| valid_column(pos)) {
                    return Err("Ticking area center is outside the world");
                }
            }
            Self::Rectangle { min, max } => {
                if !min.into_iter().chain(max).all(valid_column)
                    || min[0] > max[0]
                    || min[1] > max[1]
                {
                    return Err("Ticking area bounds are outside the world");
                }
                let width = i64::from(max[0]) - i64::from(min[0]) + 1;
                let depth = i64::from(max[1]) - i64::from(min[1]) + 1;
                if width * depth > MAX_RECTANGLE_COLUMNS {
                    return Err("Ticking areas may contain at most 100 chunks");
                }
            }
        }
        Ok(())
    }

    fn bounds(self) -> ([i32; 2], [i32; 2]) {
        match self {
            Self::Rectangle { min, max } => (min, max),
            Self::Circle { center, radius } => (
                center.map(|pos| (pos - radius).max(MIN_COLUMN)),
                center.map(|pos| (pos + radius).min(MAX_COLUMN)),
            ),
        }
    }

    #[must_use]
    pub fn contains(self, pos: Vector2<i32>) -> bool {
        let (min, max) = self.bounds();
        (min[0]..=max[0]).contains(&pos.x) && (min[1]..=max[1]).contains(&pos.y)
    }

    pub fn columns(self) -> impl Iterator<Item = Vector2<i32>> {
        let (min, max) = self.bounds();
        (min[0]..=max[0]).flat_map(move |x| (min[1]..=max[1]).map(move |z| Vector2::new(x, z)))
    }
}

fn valid_column(pos: i32) -> bool {
    (MIN_COLUMN..=MAX_COLUMN).contains(&pos)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TickingArea {
    pub name: String,
    pub shape: AreaShape,
    pub preload: bool,
}

#[must_use]
pub fn same_name(left: &str, right: &str) -> bool {
    left.to_lowercase() == right.to_lowercase()
}

#[must_use]
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(char::is_control)
}

pub type TickingAreaDimensions = BTreeMap<String, Vec<TickingArea>>;

pub fn read(root: &Path) -> io::Result<TickingAreaDimensions> {
    let path = root.join(FILE_NAME);
    match fs::read(&path) {
        Ok(data) => parse_areas(&data).map_err(|error| invalid_data(&path, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok(TickingAreaDimensions::default())
        }
        Err(error) => Err(error),
    }
}

pub fn write(root: &Path, dimensions: &TickingAreaDimensions) -> io::Result<()> {
    let path = root.join(FILE_NAME);
    validate(dimensions).map_err(|error| invalid_data(&path, error))?;
    let bytes =
        serde_json::to_vec_pretty(dimensions).map_err(|error| invalid_data(&path, error))?;
    write_file_atomically(&path, &bytes, true)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))
}

fn invalid_data(path: &Path, error: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {error}", path.display()),
    )
}

fn parse_areas(bytes: &[u8]) -> Result<TickingAreaDimensions, String> {
    let dimensions: TickingAreaDimensions =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    validate(&dimensions)?;
    Ok(dimensions)
}

pub fn validate(dimensions: &TickingAreaDimensions) -> Result<(), String> {
    if dimensions.values().map(Vec::len).sum::<usize>() > MAX_AREAS {
        return Err("Too many saved ticking areas".to_owned());
    }
    for areas in dimensions.values() {
        let mut names = FxHashSet::default();
        for area in areas {
            area.shape.validate()?;
            if !valid_name(&area.name) || !names.insert(area.name.to_lowercase()) {
                return Err("Invalid or repeated ticking area name".to_owned());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticking_areas_validate_and_cover_column_boundaries() {
        let center = Vector2::new(-1, 0);
        let one = AreaShape::circle(center, 1);
        assert_eq!(one.columns().count(), 9);
        assert!(one.contains(Vector2::new(-2, -1)));
        assert!(!one.contains(Vector2::new(1, 0)));
        assert_eq!(
            AreaShape::circle(center, 0).columns().collect::<Vec<_>>(),
            vec![center]
        );
        assert!(AreaShape::circle(center, 5).validate().is_err());
        let left = Vector2::new(-10, -10);
        assert!(
            AreaShape::rectangle(center.add_raw(0, -1), left)
                .validate()
                .is_ok()
        );
        assert!(AreaShape::rectangle(center, left).validate().is_err());
    }

    #[test]
    fn ticking_area_file_rejects_invalid_or_repeated_local_areas() {
        let area = TickingArea {
            name: "Station".to_owned(),
            shape: AreaShape::circle(Vector2::new(4, -7), 2),
            preload: true,
        };
        let mut dimensions = TickingAreaDimensions::from([
            ("minecraft:overworld".to_owned(), vec![area.clone()]),
            ("minecraft:the_nether".to_owned(), vec![area.clone()]),
        ]);
        let folder = tempfile::tempdir().unwrap();
        write(folder.path(), &dimensions).unwrap();
        let original = read(folder.path()).unwrap();
        assert_eq!(original, dimensions);
        let bytes = serde_json::to_vec(&dimensions).unwrap();
        assert_eq!(parse_areas(&bytes).unwrap(), dimensions);
        let areas = dimensions.get_mut("minecraft:the_nether").unwrap();
        areas.push(TickingArea {
            name: "station".to_owned(),
            ..area
        });
        let bytes = serde_json::to_vec(&dimensions).unwrap();
        assert!(parse_areas(&bytes).is_err());
        assert!(write(folder.path(), &dimensions).is_err());
        assert_eq!(read(folder.path()).unwrap(), original);
    }
}
