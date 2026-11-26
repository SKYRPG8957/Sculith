//! Persistent Bedrock ticking areas share a capacity within one level folder.

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use rustc_hash::FxHashSet;
use sculith_util::math::vector2::Vector2;
use sculith_world::world_info::ticking_areas::{
    self as metadata, TickingAreaDimensions, same_name, valid_name,
};
pub use sculith_world::world_info::ticking_areas::{AreaShape, MAX_AREAS, TickingArea};

use super::World;

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

#[derive(Default)]
struct SavedAreas {
    dimensions: TickingAreaDimensions,
    dirty: bool,
}

pub struct TickingAreas {
    root: PathBuf,
    data: Arc<Mutex<SavedAreas>>,
    save_order: Arc<tokio::sync::Mutex<()>>,
}

impl TickingAreas {
    pub(super) fn empty(root: &Path) -> Self {
        Self::new(root.to_path_buf(), TickingAreaDimensions::default())
    }

    pub(crate) fn read(root: &Path) -> io::Result<Self> {
        Ok(Self::new(root.to_path_buf(), metadata::read(root)?))
    }

    fn new(root: PathBuf, dimensions: TickingAreaDimensions) -> Self {
        Self {
            root,
            data: Arc::new(Mutex::new(SavedAreas {
                dimensions,
                dirty: false,
            })),
            save_order: Arc::default(),
        }
    }

    async fn save(&self) -> Result<(), String> {
        let writer = self.save_order.clone().lock_owned().await;
        let root = self.root.clone();
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
            metadata::write(&root, &snapshot)?;
            let mut data = data.lock().unwrap_or_else(PoisonError::into_inner);
            if data.dimensions == snapshot {
                data.dirty = false;
            }
            Ok::<_, io::Error>(())
        })
        .await
        .map_err(|error| format!("Ticking area save task failed: {error}"))?
        .map_err(|error| format!("Failed to save ticking areas: {error}"))
    }
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

    pub(crate) fn list_ticking_areas(
        &self,
        all_dimensions: bool,
    ) -> (TickingAreaDimensions, usize) {
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
