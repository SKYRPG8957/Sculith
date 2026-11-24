use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};

use arc_swap::ArcSwap;
use pumpkin_data::entity::EntityType;
use rustc_hash::{FxHashMap, FxHashSet};
use sculith_util::math::boundingbox::BoundingBox;
use uuid::Uuid;

use crate::entity::EntityBase;
use crate::entity::mob::has_game_event_listener;
use crate::entity::player::Player;
use crossbeam::atomic::AtomicCell;

use super::World;

type Registry = Vec<Arc<dyn EntityBase>>;
type PlayerRegistry = Vec<Arc<Player>>;
type Cell = [i32; 3];

const CELL_SIZE: f64 = 4.0;
// This limits index allocation, not entity size or query coverage.
const MAX_ENTITY_CELLS: u64 = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
struct CellRange {
    min: Cell,
    max: Cell,
}

impl CellRange {
    fn of(area: &BoundingBox) -> Option<Self> {
        let min = [area.min.x, area.min.y, area.min.z];
        let max = [area.max.x, area.max.y, area.max.z];
        if min
            .iter()
            .zip(max)
            .any(|(min, max)| !min.is_finite() || !max.is_finite() || *min > max)
        {
            return None;
        }
        Some(Self {
            min: min.map(|n| (n / CELL_SIZE).floor() as i32),
            max: max.map(|n| (n / CELL_SIZE).floor() as i32),
        })
    }

    fn count(self) -> u64 {
        self.min
            .into_iter()
            .zip(self.max)
            .fold(1u64, |count, (min, max)| {
                count.saturating_mul((i64::from(max) - i64::from(min) + 1) as u64)
            })
    }

    fn cells(self) -> impl Iterator<Item = Cell> {
        (self.min[0]..=self.max[0]).flat_map(move |x| {
            (self.min[1]..=self.max[1])
                .flat_map(move |y| (self.min[2]..=self.max[2]).map(move |z| [x, y, z]))
        })
    }

    fn contains(self, cell: Cell) -> bool {
        (0..3).all(|axis| self.min[axis] <= cell[axis] && cell[axis] <= self.max[axis])
    }

    fn indexed_members(
        self,
        cells: &FxHashMap<Cell, Vec<usize>>,
    ) -> impl Iterator<Item = usize> + '_ {
        self.cells()
            .filter_map(|cell| cells.get(&cell))
            .flatten()
            .copied()
    }
}

#[derive(Default)]
struct Index {
    registry: Weak<Registry>,
    slots: FxHashMap<i32, usize>,
    uuids: FxHashMap<Uuid, usize>,
    ranges: Vec<Option<CellRange>>,
    /// Copy of each entity's bounds, so exact tests skip the entity itself.
    bounds: Vec<AtomicCell<BoundingBox>>,
    types: Vec<&'static EntityType>,
    /// Mobs in registry order. A block update can use this after releasing the index lock.
    mobs: Arc<[usize]>,
    /// Mobs with game event listeners, in list order.
    listeners: Vec<usize>,
    cells: FxHashMap<Cell, Vec<usize>>,
    oversized: Vec<usize>,
}

impl Index {
    fn matches(&self, registry: &Arc<Registry>) -> bool {
        self.registry.as_ptr() == Arc::as_ptr(registry)
    }

    fn rebuild(&mut self, registry: &Arc<Registry>) {
        self.slots.clear();
        self.uuids.clear();
        self.ranges.clear();
        self.cells.clear();
        self.oversized.clear();
        self.types.clear();
        self.bounds.clear();
        self.listeners.clear();
        let mut mobs = Vec::new();
        for (slot, registered) in registry.iter().enumerate() {
            let entity = registered.get_entity();
            self.slots.insert(entity.entity_id, slot);
            // The first in list order wins, like a linear search.
            self.uuids.entry(entity.entity_uuid).or_insert(slot);
            self.types.push(entity.entity_type);
            if let Some(mob) = registered.get_mob() {
                mobs.push(slot);
                if has_game_event_listener(mob) {
                    self.listeners.push(slot);
                }
            }
            let bounds = entity.bounding_box.load();
            self.bounds.push(AtomicCell::new(bounds));
            let range = Self::entity_range(&bounds);
            self.ranges.push(range);
            self.insert(slot, range);
        }
        self.mobs = mobs.into();
        self.registry = Arc::downgrade(registry);
    }

    fn entity_range(area: &BoundingBox) -> Option<CellRange> {
        CellRange::of(area).filter(|range| range.count() <= MAX_ENTITY_CELLS)
    }

    fn insert(&mut self, slot: usize, range: Option<CellRange>) {
        if let Some(range) = range {
            for cell in range.cells() {
                self.cells.entry(cell).or_default().push(slot);
            }
        } else {
            self.oversized.push(slot);
        }
    }

    fn update(&mut self, entity_id: i32, area: &BoundingBox) {
        let Some(&slot) = self.slots.get(&entity_id) else {
            return;
        };
        self.bounds[slot].store(*area);
        let next = Self::entity_range(area);
        let previous = self.ranges[slot];
        if previous == next {
            return;
        }
        if let Some(previous) = previous {
            for cell in previous.cells() {
                if let std::collections::hash_map::Entry::Occupied(mut entry) =
                    self.cells.entry(cell)
                {
                    entry.get_mut().retain(|member| *member != slot);
                    if entry.get().is_empty() {
                        entry.remove();
                    }
                }
            }
        } else {
            self.oversized.retain(|member| *member != slot);
        }
        self.ranges[slot] = next;
        self.insert(slot, next);
    }

    fn intersects(&self, slot: usize, area: &BoundingBox) -> bool {
        area.intersects(&self.bounds[slot].load())
    }

    /// Slots in registry order whose type passes `matches` and whose cells touch `area`.
    fn candidates(
        &self,
        area: &BoundingBox,
        matches: impl Fn(&'static EntityType) -> bool,
    ) -> Vec<usize> {
        let matches = |slot: &usize| matches(self.types[*slot]);
        let Some(range) = CellRange::of(area) else {
            return (0..self.types.len()).filter(matches).collect();
        };
        let mut slots: Vec<usize> = self.oversized.iter().copied().filter(matches).collect();
        if range.count() <= self.cells.len() as u64 {
            for cell in range.cells() {
                if let Some(members) = self.cells.get(&cell) {
                    slots.extend(members.iter().copied().filter(matches));
                }
            }
        } else {
            for (&cell, members) in &self.cells {
                if range.contains(cell) {
                    slots.extend(members.iter().copied().filter(matches));
                }
            }
        }
        // Keep the registry order, including entities spanning several cells.
        slots.sort_unstable();
        slots.dedup();
        slots
    }
}

#[derive(Default)]
struct PlayerIndex {
    registry: Weak<PlayerRegistry>,
    slots: FxHashMap<i32, usize>,
    uuids: FxHashMap<Uuid, usize>,
    ranges: Vec<Option<CellRange>>,
    bounds: Vec<AtomicCell<BoundingBox>>,
    cells: FxHashMap<Cell, Vec<usize>>,
    oversized: Vec<usize>,
}

impl PlayerIndex {
    fn matches(&self, registry: &Arc<PlayerRegistry>) -> bool {
        self.registry.as_ptr() == Arc::as_ptr(registry)
    }

    fn rebuild(&mut self, registry: &Arc<PlayerRegistry>) {
        self.slots.clear();
        self.uuids.clear();
        self.ranges.clear();
        self.cells.clear();
        self.oversized.clear();
        self.bounds.clear();
        for (slot, player) in registry.iter().enumerate() {
            let entity = player.get_entity();
            self.slots.insert(entity.entity_id, slot);
            self.uuids.entry(player.gameprofile.id).or_insert(slot);
            let bounds = entity.bounding_box.load();
            self.bounds.push(AtomicCell::new(bounds));
            let range = Index::entity_range(&bounds);
            self.ranges.push(range);
            self.insert(slot, range);
        }
        self.registry = Arc::downgrade(registry);
    }

    fn insert(&mut self, slot: usize, range: Option<CellRange>) {
        if let Some(range) = range {
            for cell in range.cells() {
                self.cells.entry(cell).or_default().push(slot);
            }
        } else {
            self.oversized.push(slot);
        }
    }

    fn update(&mut self, entity_id: i32, area: &BoundingBox) {
        let Some(&slot) = self.slots.get(&entity_id) else {
            return;
        };
        self.bounds[slot].store(*area);
        let next = Index::entity_range(area);
        let previous = self.ranges[slot];
        if previous == next {
            return;
        }
        if let Some(previous) = previous {
            for cell in previous.cells() {
                if let std::collections::hash_map::Entry::Occupied(mut entry) =
                    self.cells.entry(cell)
                {
                    entry.get_mut().retain(|member| *member != slot);
                    if entry.get().is_empty() {
                        entry.remove();
                    }
                }
            }
        } else {
            self.oversized.retain(|member| *member != slot);
        }
        self.ranges[slot] = next;
        self.insert(slot, next);
    }

    fn intersects(&self, slot: usize, area: &BoundingBox) -> bool {
        area.intersects(&self.bounds[slot].load())
    }

    fn candidates(&self, area: &BoundingBox) -> Vec<usize> {
        let Some(range) = CellRange::of(area) else {
            return (0..self.bounds.len()).collect();
        };
        let mut slots: Vec<usize> = self.oversized.clone();
        if range.count() <= self.cells.len() as u64 {
            for cell in range.cells() {
                if let Some(members) = self.cells.get(&cell) {
                    slots.extend(members.iter().copied());
                }
            }
        } else {
            for (&cell, members) in &self.cells {
                if range.contains(cell) {
                    slots.extend(members.iter().copied());
                }
            }
        }
        slots.sort_unstable();
        slots.dedup();
        slots
    }
}

/// Live broad phase for the world's non-player entity queries.
#[derive(Default)]
pub struct EntityIndex {
    inner: RwLock<Index>,
    players: RwLock<PlayerIndex>,
    pub(super) registering: Mutex<FxHashSet<Uuid>>,
}

impl EntityIndex {
    /// Regions can publish registry changes while the world is busy. Use the current
    /// generation even when a world query has not refreshed this index yet.
    fn try_read_current<'index, T, R>(
        lock: &'index RwLock<T>,
        source: &ArcSwap<R>,
        matches: impl Fn(&T, &Arc<R>) -> bool,
        rebuild: impl FnOnce(&mut T, &Arc<R>),
    ) -> Option<(Arc<R>, RwLockReadGuard<'index, T>)> {
        let index = lock.try_read().ok()?;
        let registry = source.load_full();
        if matches(&index, &registry) {
            return Some((registry, index));
        }
        drop(index);
        let mut index = lock.try_write().ok()?;
        let registry = source.load_full();
        if !matches(&index, &registry) {
            rebuild(&mut index, &registry);
        }
        Some((registry, RwLockWriteGuard::downgrade(index)))
    }

    fn with_index<T>(&self, registry: &Arc<Registry>, query: impl FnOnce(&Index) -> T) -> T {
        let index = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if index.matches(registry) {
            return query(&index);
        }
        drop(index);
        let mut index = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !index.matches(registry) {
            index.rebuild(registry);
        }
        query(&index)
    }

    fn with_players<T>(
        &self,
        registry: &Arc<PlayerRegistry>,
        query: impl FnOnce(&PlayerIndex) -> T,
    ) -> T {
        let index = self
            .players
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if index.matches(registry) {
            return query(&index);
        }
        drop(index);
        let mut index = self
            .players
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !index.matches(registry) {
            index.rebuild(registry);
        }
        query(&index)
    }

    pub fn query(
        &self,
        registry: Arc<Registry>,
        area: BoundingBox,
    ) -> impl Iterator<Item = Arc<dyn EntityBase>> + use<> {
        self.query_types(registry, area, |_| true)
    }

    /// Like [`Self::query`], limited to entity types that pass `matches`.
    pub fn query_types<F: Fn(&'static EntityType) -> bool>(
        &self,
        registry: Arc<Registry>,
        area: BoundingBox,
        matches: F,
    ) -> impl Iterator<Item = Arc<dyn EntityBase>> + use<F> {
        let slots = self.with_index(&registry, |index| {
            let mut slots = index.candidates(&area, matches);
            slots.retain(|&slot| index.intersects(slot, &area));
            slots
        });
        slots.into_iter().map(move |slot| registry[slot].clone())
    }

    pub fn query_players(
        &self,
        registry: Arc<PlayerRegistry>,
        area: BoundingBox,
    ) -> impl Iterator<Item = Arc<Player>> + use<> {
        let slots = self.with_players(&registry, |index| {
            let mut slots = index.candidates(&area);
            slots.retain(|&slot| index.intersects(slot, &area));
            slots
        });
        slots.into_iter().map(move |slot| registry[slot].clone())
    }

    /// A block update can still use the live registry when the index is busy or outdated.
    pub(super) fn cached_mob_slots(&self, registry: &Arc<Registry>) -> Option<Arc<[usize]>> {
        let index = self.inner.try_read().ok()?;
        index.matches(registry).then(|| index.mobs.clone())
    }

    pub(super) fn try_has_game_event_listener(
        &self,
        source: &ArcSwap<Registry>,
        area: &BoundingBox,
    ) -> Option<bool> {
        if source.load().is_empty() {
            return Some(false);
        }
        let (registry, index) =
            Self::try_read_current(&self.inner, source, Index::matches, Index::rebuild)?;
        let has_listener = index
            .listeners
            .iter()
            .any(|&slot| index.intersects(slot, area));
        Arc::ptr_eq(&registry, &source.load()).then_some(has_listener)
    }

    /// Mobs with game event listeners whose body overlaps `area`, in list order.
    pub fn listeners(
        &self,
        registry: Arc<Registry>,
        area: BoundingBox,
    ) -> impl Iterator<Item = Arc<dyn EntityBase>> + use<> {
        let slots: Vec<usize> = self.with_index(&registry, |index| {
            index
                .listeners
                .iter()
                .copied()
                .filter(|&slot| index.intersects(slot, &area))
                .collect()
        });
        slots.into_iter().map(move |slot| registry[slot].clone())
    }

    pub fn by_id(&self, registry: &Arc<Registry>, id: i32) -> Option<Arc<dyn EntityBase>> {
        let slot = self.with_index(registry, |index| index.slots.get(&id).copied())?;
        Some(registry[slot].clone())
    }

    /// The first entity with `uuid` in registry order, without a scan.
    pub fn by_uuid(&self, registry: &Arc<Registry>, uuid: Uuid) -> Option<Arc<dyn EntityBase>> {
        let slot = self.with_index(registry, |index| index.uuids.get(&uuid).copied())?;
        Some(registry[slot].clone())
    }

    pub fn player_by_id(&self, registry: &Arc<PlayerRegistry>, id: i32) -> Option<Arc<Player>> {
        let slot = self.with_players(registry, |index| index.slots.get(&id).copied())?;
        Some(registry[slot].clone())
    }

    pub fn player_by_uuid(
        &self,
        registry: &Arc<PlayerRegistry>,
        uuid: Uuid,
    ) -> Option<Arc<Player>> {
        let slot = self.with_players(registry, |index| index.uuids.get(&uuid).copied())?;
        Some(registry[slot].clone())
    }

    pub fn set_bounds(&self, entity_id: i32, bounds: &AtomicCell<BoundingBox>, area: BoundingBox) {
        let index = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next = Index::entity_range(&area);
        match index.slots.get(&entity_id) {
            None => {
                bounds.store(area);
                return;
            }
            Some(&slot) if index.ranges[slot] == next => {
                index.bounds[slot].store(area);
                bounds.store(area);
                return;
            }
            Some(_) => {}
        }
        drop(index);
        let mut index = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        index.update(entity_id, &area);
        bounds.store(area);
    }

    /// Pins both current indices while committing an isolated movement.
    /// The caller holds the actor's bounds owner exclusively and publishes the bounds in `apply`.
    pub fn try_move_isolated_drop(
        &self,
        world: &World,
        entity_id: i32,
        swept: BoundingBox,
        player_clearance: BoundingBox,
        bounds: BoundingBox,
        apply: impl FnOnce() -> bool,
    ) {
        let Some((players, player_index)) = Self::try_read_current(
            &self.players,
            &world.players,
            PlayerIndex::matches,
            PlayerIndex::rebuild,
        ) else {
            return;
        };
        let Some((entities, index)) =
            Self::try_read_current(&self.inner, &world.entities, Index::matches, Index::rebuild)
        else {
            return;
        };
        let registries_unchanged = || {
            Arc::ptr_eq(&entities, &world.entities.load())
                && Arc::ptr_eq(&players, &world.players.load())
        };
        let Some(&own_slot) = index.slots.get(&entity_id) else {
            return;
        };
        let next_range = Index::entity_range(&bounds);
        if let Some(swept_range) = Index::entity_range(&swept)
            && let Some(player_range) = Index::entity_range(&player_clearance)
            && next_range.is_some()
            && next_range == index.ranges[own_slot]
            && index.oversized.is_empty()
            && player_index.oversized.is_empty()
            && swept_range
                .indexed_members(&index.cells)
                .all(|slot| slot == own_slot)
            && player_range
                .indexed_members(&player_index.cells)
                .next()
                .is_none()
        {
            // A same-cell bounds update cannot introduce a new candidate while membership is pinned.
            if registries_unchanged() && apply() {
                index.bounds[own_slot].store(bounds);
            }
            return;
        }
        drop(index);
        drop(player_index);

        // Nearby candidates can change their exact bounds under a shared membership lock.
        let (Ok(player_index), Ok(mut index)) = (self.players.try_write(), self.inner.try_write())
        else {
            return;
        };
        if !index.matches(&entities) || !player_index.matches(&players) {
            return;
        }
        let Some(&own_slot) = index.slots.get(&entity_id) else {
            return;
        };
        if index
            .candidates(&swept, |_| true)
            .into_iter()
            .any(|slot| slot != own_slot && index.intersects(slot, &swept))
            || player_index
                .candidates(&player_clearance)
                .into_iter()
                .any(|slot| player_index.intersects(slot, &player_clearance))
        {
            return;
        }
        if registries_unchanged() && apply() {
            index.update(entity_id, &bounds);
        }
    }

    pub(crate) fn try_move_regional_player<T>(
        &self,
        world: &World,
        player: &Player,
        bounds: BoundingBox,
        clear_area: Option<BoundingBox>,
        apply: impl FnOnce() -> Option<T>,
    ) -> Option<T> {
        let mut index = self.players.try_write().ok()?;
        let players = world.players.load_full();
        if !index.matches(&players) {
            index.rebuild(&players);
        }
        let entity_id = player.get_entity().entity_id;
        let &slot = index.slots.get(&entity_id)?;
        let active_actors = if let Some(area) = clear_area {
            let mut actors = self.inner.try_write().ok()?;
            let entities = world.entities.load_full();
            if !actors.matches(&entities) {
                actors.rebuild(&entities);
            }
            if actors
                .candidates(&area, |_| true)
                .into_iter()
                .any(|candidate| actors.intersects(candidate, &area))
                || index
                    .candidates(&area)
                    .into_iter()
                    .any(|candidate| candidate != slot && index.intersects(candidate, &area))
            {
                return None;
            }
            Some((entities, actors))
        } else {
            None
        };
        if !std::ptr::eq(players[slot].as_ref(), player)
            || !Arc::ptr_eq(&players, &world.players.load())
            || active_actors
                .as_ref()
                .is_some_and(|(entities, _)| !Arc::ptr_eq(entities, &world.entities.load()))
        {
            return None;
        }
        let result = apply()?;
        index.update(entity_id, &bounds);
        Some(result)
    }

    /// Pins the cell memberships of an isolated drop and its only possible collector.
    pub(super) fn try_with_pickup_contact<R>(
        &self,
        world: &World,
        item: i32,
        area: &BoundingBox,
        collect: impl FnOnce(&Player, &dyn Fn() -> bool) -> Option<R>,
    ) -> Option<R> {
        let range = Index::entity_range(area)?;
        let (players, player_index) = Self::try_read_current(
            &self.players,
            &world.players,
            PlayerIndex::matches,
            PlayerIndex::rebuild,
        )?;
        let mut candidates = range.indexed_members(&player_index.cells);
        let collector = candidates.next()?;
        if !player_index.oversized.is_empty()
            || candidates.any(|slot| slot != collector)
            || !player_index.intersects(collector, area)
        {
            return None;
        }
        let (entities, index) =
            Self::try_read_current(&self.inner, &world.entities, Index::matches, Index::rebuild)?;
        let &source = index.slots.get(&item)?;
        // A same-cell bounds change can introduce a contact under the read guard.
        // Reserve the whole cells, including possible vehicles, merges and mob collectors.
        if !index.oversized.is_empty()
            || range
                .indexed_members(&index.cells)
                .any(|slot| slot != source && !entities[slot].get_entity().is_removed())
        {
            return None;
        }
        collect(&players[collector], &|| {
            Arc::ptr_eq(&players, &world.players.load())
                && Arc::ptr_eq(&entities, &world.entities.load())
        })
    }

    pub fn set_player_bounds(
        &self,
        entity_id: i32,
        bounds: &AtomicCell<BoundingBox>,
        area: BoundingBox,
    ) {
        let index = self
            .players
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next = Index::entity_range(&area);
        match index.slots.get(&entity_id) {
            None => {
                bounds.store(area);
                return;
            }
            Some(&slot) if index.ranges[slot] == next => {
                index.bounds[slot].store(area);
                bounds.store(area);
                return;
            }
            Some(_) => {}
        }
        drop(index);
        let mut index = self
            .players
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        index.update(entity_id, &area);
        bounds.store(area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{
        Entity,
        ai::test_support::world,
        interaction::InteractionEntity,
        item::ItemEntity,
        passive::{allay::AllayEntity, cow::CowEntity},
    };
    use crate::server::TickDeadline;
    use pumpkin_data::{entity::EntityType, item::Item, item_stack::ItemStack};
    use pumpkin_world::chunk::ChunkData;
    use sculith_util::math::{position::BlockPos, vector3::Vector3};
    use std::time::{Duration, Instant};

    fn ids(world: &super::super::World, area: &BoundingBox) -> Vec<i32> {
        world
            .get_entities_at_box(area)
            .iter()
            .map(|entity| entity.get_entity().entity_id)
            .collect()
    }

    #[tokio::test]
    async fn mob_slots_follow_registry_generation_and_skip_busy_indices()
    -> Result<(), Box<dyn std::error::Error>> {
        let folder = tempfile::tempdir()?;
        let world = world(&folder);
        let position = Vector3::new(0.5, 64.0, 0.5);
        let cow = CowEntity::new(Entity::new(world.clone(), position, &EntityType::COW));
        let marker = InteractionEntity::new(Entity::new(
            world.clone(),
            position,
            &EntityType::INTERACTION,
        ));
        world
            .entities
            .store(Arc::new(vec![marker.clone(), cow.clone()]));
        let original = world.entities.load_full();
        let index = &world.entity_index;
        let cow_id = cow.get_entity().entity_id;
        assert!(index.cached_mob_slots(&original).is_none());
        assert!(world.get_entity_by_id(cow_id).is_some());
        let slots = index
            .cached_mob_slots(&original)
            .ok_or("missing mob slots")?;
        assert_eq!(slots.as_ref(), &[1]);

        world.entities.store(Arc::new(vec![cow, marker]));
        let current = world.entities.load_full();
        assert!(index.cached_mob_slots(&current).is_none());
        assert!(world.get_entity_by_id(cow_id).is_some());
        assert!(index.cached_mob_slots(&original).is_none());
        assert_eq!(index.cached_mob_slots(&current).as_deref(), Some(&[0][..]));
        assert_eq!(slots.as_ref(), &[1]);
        let writer = index
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(index.cached_mob_slots(&current).is_none());
        drop(writer);
        world.level.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn game_event_query_refreshes_the_listener_index_when_available()
    -> Result<(), Box<dyn std::error::Error>> {
        let folder = tempfile::tempdir()?;
        let world = world(&folder);
        let position = Vector3::new(0.5, 64.0, 0.5);
        let allay = AllayEntity::new(Entity::new(world.clone(), position, &EntityType::ALLAY));
        let cow = CowEntity::new(Entity::new(world.clone(), position, &EntityType::COW));
        let area = allay.get_entity().bounding_box.load();
        let index = &world.entity_index;
        let query = || index.try_has_game_event_listener(&world.entities, &area);

        world.entities.store(Arc::new(vec![cow.clone()]));
        assert_eq!(query(), Some(false));

        let reader = index
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        world.entities.store(Arc::new(vec![cow.clone(), allay]));
        assert_eq!(query(), None);
        drop(reader);
        assert_eq!(query(), Some(true));

        world.entities.store(Arc::new(vec![cow]));
        assert_eq!(query(), Some(false));
        world.level.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn isolated_items_can_move_while_both_indices_have_readers()
    -> Result<(), Box<dyn std::error::Error>> {
        let folder = tempfile::tempdir()?;
        let world = world(&folder);
        let items = [-10.0, 102.0].map(|x| {
            let position = Vector3::new(x, 66.0, 2.0);
            let chunk = BlockPos::floored_v(position).chunk_position();
            world
                .level
                .loaded_chunks
                .insert(chunk, ChunkData::empty_sync(chunk.x, chunk.y));
            Arc::new(ItemEntity::new_with_velocity(
                Entity::new(world.clone(), position, &EntityType::ITEM),
                ItemStack::new(1, &Item::STONE),
                Vector3::new(0.3, -0.2, 0.0),
                ItemEntity::DEFAULT_PICKUP_DELAY,
            ))
        });
        world
            .entities
            .store(Arc::new(vec![items[0].clone(), items[1].clone()]));
        let old_bounds = items
            .each_ref()
            .map(|item| item.get_entity().bounding_box.load());
        assert_eq!(
            ids(&world, &old_bounds[0]),
            [items[0].get_entity().entity_id]
        );
        assert!(world.get_players_at_box(&old_bounds[0]).is_empty());

        // The old exclusive path could not move either item with these readers active.
        let entities = world
            .entity_index
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let players = world
            .entity_index
            .players
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let deadline = TickDeadline {
            at: Instant::now(),
            period: Duration::from_millis(50),
        };
        let started = std::sync::Barrier::new(items.len());
        std::thread::scope(|threads| {
            for (id, item) in items.iter().enumerate() {
                let world = &world;
                let started = &started;
                threads.spawn(move || {
                    let chunk = item.get_entity().chunk_pos.load();
                    started.wait();
                    item.try_regional_motion(
                        world,
                        chunk,
                        id,
                        deadline,
                        &|pos| pos == chunk,
                        || {},
                    );
                });
            }
        });
        drop(players);
        drop(entities);

        for (item, old) in items.iter().zip(old_bounds) {
            let entity = item.get_entity();
            let moved = entity.bounding_box.load();
            assert!(moved.min.x > old.max.x);
            assert!(ids(&world, &old).is_empty());
            assert_eq!(ids(&world, &moved), [entity.entity_id]);
        }
        world.level.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn regional_motion_uses_new_registries_before_a_world_query()
    -> Result<(), Box<dyn std::error::Error>> {
        let folder = tempfile::tempdir()?;
        let world = world(&folder);
        let start = Vector3::new(102.0, 66.0, 2.0);
        let chunk = BlockPos::floored_v(start).chunk_position();
        world
            .level
            .loaded_chunks
            .insert(chunk, ChunkData::empty_sync(chunk.x, chunk.y));
        let item = Arc::new(ItemEntity::new_with_velocity(
            Entity::new(world.clone(), start, &EntityType::ITEM),
            ItemStack::new(1, &Item::STONE),
            Vector3::new(0.3, -0.2, 0.0),
            ItemEntity::DEFAULT_PICKUP_DELAY,
        ));
        let entity = item.get_entity();
        let original = entity.bounding_box.load();
        let neighbor = InteractionEntity::new(Entity::new(
            world.clone(),
            start + Vector3::new(0.7, 0.0, 0.0),
            &EntityType::INTERACTION,
        ));
        world.entities.store(Arc::new(vec![item.clone()]));
        assert_eq!(ids(&world, &original), [entity.entity_id]);
        assert!(world.get_players_at_box(&original).is_empty());

        // A newly registered neighbor must prevent isolated movement.
        world.entities.store(Arc::new(vec![item.clone(), neighbor]));
        let deadline = TickDeadline {
            at: Instant::now(),
            period: Duration::from_millis(50),
        };
        let mut published = false;
        item.try_regional_motion(&world, chunk, 0, deadline, &|pos| pos == chunk, || {
            published = true;
        });
        assert!(!published);
        assert_eq!(entity.pos.load(), start);
        let current = {
            let index = world
                .entity_index
                .inner
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            index.matches(&world.entities.load())
        };
        assert!(current);

        // A regional expiry can replace the registry without a coordinated world tick.
        world.entities.store(Arc::new(vec![item.clone()]));
        world.players.store(Arc::default());
        item.try_regional_motion(&world, chunk, 0, deadline, &|pos| pos == chunk, || {
            published = true;
        });
        assert!(published);
        let moved = entity.bounding_box.load();
        assert!(moved.min.x > original.max.x);
        assert!(ids(&world, &original).is_empty());
        assert_eq!(ids(&world, &moved), [entity.entity_id]);
        world.level.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn queries_follow_moves_resizes_and_registry_replacement()
    -> Result<(), Box<dyn std::error::Error>> {
        let folder = tempfile::tempdir()?;
        let world = world(&folder);
        let body = CowEntity::new(Entity::new(
            world.clone(),
            Vector3::new(-4.7, 64.0, 0.0),
            &EntityType::COW,
        ));
        world.entities.store(Arc::new(vec![body.clone()]));
        let area = BoundingBox::new(
            Vector3::new(-4.2, 64.0, -0.1),
            Vector3::new(-4.1, 65.0, 0.1),
        );
        assert!(ids(&world, &area).is_empty());
        assert!(
            world
                .get_entity_by_uuid(body.get_entity().entity_uuid)
                .is_some()
        );
        // A bound change within the same grid cells still changes exact membership.
        body.get_entity().set_pos(Vector3::new(-4.6, 64.0, 0.0));
        assert_eq!(ids(&world, &area), [body.get_entity().entity_id]);
        body.get_entity().set_pos(Vector3::new(8.0, 64.0, 0.0));
        assert!(ids(&world, &area).is_empty());
        body.get_entity().bounding_box.store(BoundingBox::new(
            Vector3::new(-5.0, 64.0, -13.0),
            Vector3::new(21.0, 65.0, 13.0),
        ));
        assert_eq!(ids(&world, &area), [body.get_entity().entity_id]);
        body.get_entity()
            .set_pose(pumpkin_data::entity::EntityPose::Dying);
        assert!(ids(&world, &area).is_empty());
        let replacement = InteractionEntity::new(Entity::new(
            world.clone(),
            Vector3::new(-4.0, 64.0, 0.0),
            &EntityType::INTERACTION,
        ));
        world.entities.store(Arc::new(vec![replacement.clone()]));
        assert_eq!(ids(&world, &area), [replacement.entity.entity_id]);
        assert!(
            world
                .get_entity_by_id(body.get_entity().entity_id)
                .is_none()
        );
        assert!(
            world
                .get_entity_by_uuid(body.get_entity().entity_uuid)
                .is_none()
        );
        assert!(
            world
                .get_entity_by_uuid(replacement.entity.entity_uuid)
                .is_some()
        );
        world.entities.store(Arc::new(Vec::new()));
        let removed = Arc::downgrade(&replacement);
        assert!(ids(&world, &area).is_empty());
        drop(replacement);
        assert!(removed.upgrade().is_none());
        world.level.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn queries_keep_order_and_coverage_across_cells_and_oversized_bounds()
    -> Result<(), Box<dyn std::error::Error>> {
        let folder = tempfile::tempdir()?;
        let world = world(&folder);
        let near = InteractionEntity::new(Entity::new(
            world.clone(),
            Vector3::new(-0.1, 64.0, 0.0),
            &EntityType::INTERACTION,
        ));
        let wide = InteractionEntity::new(Entity::new(
            world.clone(),
            Vector3::new(100.0, 64.0, 0.0),
            &EntityType::INTERACTION,
        ));
        wide.set_width(220.0);
        world
            .entities
            .store(Arc::new(vec![wide.clone(), near.clone()]));
        let area = BoundingBox::new(Vector3::new(-1.0, 63.0, -1.0), Vector3::new(1.0, 66.0, 1.0));
        assert_eq!(
            ids(&world, &area),
            [wide.entity.entity_id, near.entity.entity_id]
        );
        let huge = BoundingBox::new(
            Vector3::new(-1e20, -1e20, -1e20),
            Vector3::new(1e20, 1e20, 1e20),
        );
        assert_eq!(
            ids(&world, &huge),
            [wide.entity.entity_id, near.entity.entity_id]
        );
        // Registry order can change independently of bounds, including island spawn ordering.
        world
            .entities
            .store(Arc::new(vec![near.clone(), wide.clone()]));
        assert_eq!(
            ids(&world, &area),
            [near.entity.entity_id, wide.entity.entity_id]
        );
        wide.set_width(1.0);
        assert_eq!(ids(&world, &area), [near.entity.entity_id]);
        let mut found = Vec::new();
        world.extend_entities_in_box_where(&mut found, usize::MAX, area, |_| true);
        assert_eq!(
            found
                .iter()
                .map(|entity| entity.get_entity().entity_id)
                .collect::<Vec<_>>(),
            [near.entity.entity_id]
        );
        world.level.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn transferred_bounds_update_the_destination_index()
    -> Result<(), Box<dyn std::error::Error>> {
        let source_folder = tempfile::tempdir()?;
        let destination_folder = tempfile::tempdir()?;
        let source = world(&source_folder);
        let destination = world(&destination_folder);
        let body = CowEntity::new(Entity::new(
            source.clone(),
            Vector3::new(1.0, 64.0, 0.0),
            &EntityType::COW,
        ));
        let area = BoundingBox::new(Vector3::new(0.0, 63.0, -1.0), Vector3::new(2.0, 66.0, 1.0));
        source.entities.store(Arc::new(vec![body.clone()]));
        assert_eq!(ids(&source, &area), [body.get_entity().entity_id]);
        source.entities.store(Arc::new(Vec::new()));
        body.get_entity().set_world(destination.clone());
        destination.entities.store(Arc::new(vec![body.clone()]));
        assert_eq!(ids(&destination, &area), [body.get_entity().entity_id]);
        body.get_entity().set_pos(Vector3::new(20.0, 64.0, 0.0));
        assert!(ids(&source, &area).is_empty());
        assert!(ids(&destination, &area).is_empty());
        assert_eq!(
            ids(&destination, &area.expand(20.0, 0.0, 0.0)),
            [body.get_entity().entity_id]
        );
        source.level.shutdown().await;
        destination.level.shutdown().await;
        Ok(())
    }
}
