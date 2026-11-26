//! Persistent Bedrock ticking areas share a capacity within one level folder.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use rustc_hash::FxHashSet;
use sculith_util::math::vector2::Vector2;
use serde::{Deserialize, Serialize};

use super::World;

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
    pub(crate) fn rectangle(from: Vector2<i32>, to: Vector2<i32>) -> Self {
        Self::Rectangle {
            min: [from.x.min(to.x), from.y.min(to.y)],
            max: [from.x.max(to.x), from.y.max(to.y)],
        }
    }

    pub(crate) const fn circle(center: Vector2<i32>, radius: i32) -> Self {
        Self::Circle {
            center: [center.x, center.y],
            radius,
        }
    }

    fn validate(self) -> Result<(), &'static str> {
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

    fn contains(self, pos: Vector2<i32>) -> bool {
        let (min, max) = self.bounds();
        (min[0]..=max[0]).contains(&pos.x) && (min[1]..=max[1]).contains(&pos.y)
    }

    fn columns(self) -> impl Iterator<Item = Vector2<i32>> {
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
    pub(crate) name: String,
    pub(crate) shape: AreaShape,
    pub(crate) preload: bool,
}

#[derive(Clone, Copy)]
pub enum AreaSelection<'a> {
    All,
    Name(&'a str),
    Column(Vector2<i32>),
}

impl AreaSelection<'_> {
    fn matches(self, area: &TickingArea) -> bool {
        match self {
            Self::All => true,
            Self::Name(name) => same_name(name, &area.name),
            Self::Column(column) => area.shape.contains(column),
        }
    }
}

fn same_name(left: &str, right: &str) -> bool {
    left.to_lowercase() == right.to_lowercase()
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(char::is_control)
}

type Dimensions = BTreeMap<String, Vec<TickingArea>>;

#[derive(Default)]
struct SavedAreas {
    dimensions: Dimensions,
    dirty: bool,
}

pub struct TickingAreas {
    path: PathBuf,
    data: Arc<Mutex<SavedAreas>>,
    save_order: Arc<tokio::sync::Mutex<()>>,
}

impl TickingAreas {
    pub(super) fn empty(root: &Path) -> Self {
        Self::new(root.join(FILE_NAME), Dimensions::default())
    }

    pub(crate) fn read(root: &Path) -> io::Result<Self> {
        let path = root.join(FILE_NAME);
        let data = match fs::read(&path) {
            Ok(bytes) => parse_areas(&bytes).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {error}", path.display()),
                )
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Dimensions::default(),
            Err(error) => return Err(error),
        };
        Ok(Self::new(path, data))
    }

    fn new(path: PathBuf, dimensions: Dimensions) -> Self {
        Self {
            path,
            data: Arc::new(Mutex::new(SavedAreas {
                dimensions,
                dirty: false,
            })),
            save_order: Arc::default(),
        }
    }

    async fn save(&self) -> Result<(), String> {
        let writer = self.save_order.clone().lock_owned().await;
        let path = self.path.clone();
        let snapshot = {
            let data = self.data.lock().unwrap_or_else(PoisonError::into_inner);
            if !data.dirty {
                return Ok(());
            }
            data.dimensions.clone()
        };
        let data = self.data.clone();
        tokio::task::spawn_blocking(move || {
            let _writer = writer;
            // Use the same-directory replace pattern as custom world data.
            let directory = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
            fs::create_dir_all(directory)?;
            let mut pending = tempfile::NamedTempFile::new_in(directory)?;
            serde_json::to_writer_pretty(pending.as_file_mut(), &snapshot)?;
            pending.as_file().sync_all()?;
            pending.persist(path).map_err(|error| error.error)?;
            let mut data = data.lock().unwrap_or_else(PoisonError::into_inner);
            if data.dimensions == snapshot {
                data.dirty = false;
            }
            Ok::<_, io::Error>(())
        })
        .await
        .map_err(|error| format!("Ticking area save task failed: {error}"))?
        .map_err(|error| format!("Failed to save {}: {error}", self.path.display()))
    }
}

fn parse_areas(bytes: &[u8]) -> Result<Dimensions, String> {
    let dimensions: Dimensions =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
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
    Ok(dimensions)
}

impl World {
    pub(crate) fn restore_ticking_area_tickets(&self) {
        let data = self
            .ticking_areas
            .data
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(areas) = data.dimensions.get(self.dimension.minecraft_name) {
            self.replace_ticking_area_tickets(areas);
        }
    }

    pub(crate) async fn preload_ticking_areas(&self) -> io::Result<()> {
        let columns: FxHashSet<_> = {
            let data = self
                .ticking_areas
                .data
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            data.dimensions
                .get(self.dimension.minecraft_name)
                .into_iter()
                .flatten()
                .filter(|area| area.preload)
                .flat_map(|area| area.shape.columns())
                .collect()
        };
        if columns.is_empty() {
            return Ok(());
        }
        tokio::time::timeout(Duration::from_secs(180), async {
            while columns.iter().any(|pos| !self.level.is_chunk_loaded(pos)) {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "Timed out preloading ticking areas in {}",
                    self.dimension.minecraft_name
                ),
            )
        })?;
        tracing::debug!(
            target: "sculith::ticking_areas",
            dimension = self.dimension.minecraft_name,
            columns = columns.len(),
            "Preloaded ticking area columns"
        );
        Ok(())
    }

    pub(super) async fn save_ticking_areas(&self) -> Result<(), String> {
        self.ticking_areas.save().await
    }

    pub(crate) fn list_ticking_areas(&self, all_dimensions: bool) -> (Dimensions, usize) {
        let data = self
            .ticking_areas
            .data
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let total = data.dimensions.values().map(Vec::len).sum();
        let dimensions = data
            .dimensions
            .iter()
            .filter(|(key, areas)| {
                !areas.is_empty()
                    && (all_dimensions || key.as_str() == self.dimension.minecraft_name)
            })
            .map(|(key, areas)| (key.clone(), areas.clone()))
            .collect();
        (dimensions, total)
    }

    pub(crate) fn add_ticking_area(
        &self,
        name: Option<&str>,
        shape: AreaShape,
        preload: bool,
    ) -> Result<(TickingArea, usize), String> {
        shape.validate()?;
        self.edit_ticking_areas(|areas, total| {
            if total >= MAX_AREAS {
                return Err(format!("All {MAX_AREAS} ticking area slots are in use"));
            }
            let used = |name: &str| areas.iter().any(|area| same_name(&area.name, name));
            let name = match name.filter(|name| !name.is_empty()) {
                Some(name) if !valid_name(name) => {
                    return Err("Invalid ticking area name".to_owned());
                }
                Some(name) if used(name) => {
                    return Err(format!("Ticking area {name} already exists"));
                }
                Some(name) => name.to_owned(),
                None => (0..MAX_AREAS)
                    .map(|i| format!("Area{i}"))
                    .find(|name| !used(name))
                    .ok_or_else(|| "No available ticking area name".to_owned())?,
            };
            let area = TickingArea {
                name,
                shape,
                preload,
            };
            areas.push(area.clone());
            Ok((area, total + 1))
        })
    }

    pub(crate) fn remove_ticking_areas(
        &self,
        selection: AreaSelection<'_>,
    ) -> (Vec<TickingArea>, usize) {
        self.edit_ticking_areas(|areas, total| {
            let mut removed = Vec::new();
            areas.retain(|area| {
                if selection.matches(area) {
                    removed.push(area.clone());
                    false
                } else {
                    true
                }
            });
            let remaining = total - removed.len();
            (removed, remaining)
        })
    }

    pub(crate) fn set_ticking_area_preload(
        &self,
        selection: AreaSelection<'_>,
        value: Option<bool>,
    ) -> Vec<TickingArea> {
        self.edit_ticking_areas(|areas, _| {
            areas
                .iter_mut()
                .filter(|area| selection.matches(area))
                .map(|area| {
                    if let Some(value) = value {
                        area.preload = value;
                    }
                    area.clone()
                })
                .collect()
        })
    }

    fn edit_ticking_areas<T>(&self, edit: impl FnOnce(&mut Vec<TickingArea>, usize) -> T) -> T {
        let mut data = self
            .ticking_areas
            .data
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let dimension = self.dimension.minecraft_name;
        let total = data.dimensions.values().map(Vec::len).sum();
        let mut areas = data.dimensions.remove(dimension).unwrap_or_default();
        let previous = areas.clone();
        let result = edit(&mut areas, total);
        let reload = if previous == areas {
            false
        } else {
            data.dirty = true;
            self.replace_ticking_area_tickets(&areas)
        };
        if !areas.is_empty() {
            data.dimensions.insert(dimension.to_owned(), areas);
        }
        drop(data);
        if reload {
            self.update_active_chunks();
        }
        result
    }

    fn replace_ticking_area_tickets(&self, areas: &[TickingArea]) -> bool {
        let next: FxHashSet<_> = areas.iter().flat_map(|area| area.shape.columns()).collect();
        let previous = self.ticking_area_columns.load_full();
        if next == *previous {
            return false;
        }
        let mut loading = self
            .level
            .chunk_loading
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for pos in previous.difference(&next) {
            loading.remove_force_ticket(*pos);
        }
        for pos in next.difference(&previous) {
            loading.add_force_ticket(*pos);
        }
        self.ticking_area_columns.store(Arc::new(next));
        loading.send_change();
        true
    }
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
        let mut dimensions = Dimensions::from([
            ("minecraft:overworld".to_owned(), vec![area.clone()]),
            ("minecraft:the_nether".to_owned(), vec![area.clone()]),
        ]);
        let bytes = serde_json::to_vec(&dimensions).unwrap();
        assert_eq!(parse_areas(&bytes).unwrap(), dimensions);
        let areas = dimensions.get_mut("minecraft:the_nether").unwrap();
        areas.push(TickingArea {
            name: "station".to_owned(),
            ..area
        });
        let bytes = serde_json::to_vec(&dimensions).unwrap();
        assert!(parse_areas(&bytes).is_err());
    }
}
