//! Physical entity records that can still contain a dropped stack.

use std::path::{Component, Path, PathBuf};

use pumpkin_nbt::{compound::NbtCompound, tag::NbtTag};
use rustc_hash::FxHashMap;
use sculith_util::math::vector2::Vector2;
use uuid::Uuid;

use super::ItemEntity;

const RECORDS_TAG: &str = "SculithItemStorageRecords";
const SPLIT_TAG: &str = "SculithPartialItemTransfer";

#[derive(Default)]
pub(super) struct SavedItemHistory {
    records: Vec<SavedItemRecord>,
    count: Option<u8>,
    split: bool,
}

impl SavedItemHistory {
    fn check_count(&mut self, count: u8) {
        if self.count.is_some_and(|previous| count != previous) {
            self.mark_split();
        }
    }

    fn mark_split(&mut self) {
        self.records.clear();
        self.count = None;
        self.split = true;
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct SavedItemRecord {
    pub world: PathBuf,
    pub chunk: Vector2<i32>,
    pub uuid: Uuid,
}

impl SavedItemRecord {
    fn from_nbt(record: &NbtTag) -> Option<Self> {
        let record = record.extract_compound()?;
        let world = PathBuf::from(record.get_string("World")?);
        if !world
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            return None;
        }
        let NbtTag::IntArray(chunk) = record.get("Chunk")? else {
            return None;
        };
        let &[x, z] = chunk.as_slice() else {
            return None;
        };
        Some(Self {
            world,
            chunk: Vector2::new(x, z),
            uuid: record.get_uuid("UUID")?,
        })
    }

    fn to_nbt(&self) -> NbtTag {
        let mut record = NbtCompound::new();
        let world: Vec<_> = self
            .world
            .iter()
            .map(std::ffi::OsStr::to_string_lossy)
            .collect();
        record.put_string("World", world.join("/"));
        record.put("Chunk", NbtTag::IntArray(vec![self.chunk.x, self.chunk.y]));
        record.put_uuid("UUID", self.uuid);
        NbtTag::Compound(record)
    }
}

impl ItemEntity {
    pub(crate) fn saved_item_records(&self, count: u8) -> Option<Vec<SavedItemRecord>> {
        let mut history = self.saved_records.try_lock().ok()?;
        history.check_count(count);
        Some(history.records.clone())
    }

    fn storage_record(&self, world: &Path, chunk: Vector2<i32>) -> SavedItemRecord {
        SavedItemRecord {
            world: world.to_path_buf(),
            chunk,
            uuid: self.get_entity().entity_uuid,
        }
    }

    fn write_saved_item_records(&self, world: &Path, chunk: Vector2<i32>, nbt: &mut NbtCompound) {
        let Some(count) = nbt
            .get_compound("Item")
            .and_then(|item| item.get_int("count"))
            .and_then(|count| u8::try_from(count).ok())
        else {
            return;
        };
        let mut history = self
            .saved_records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        history.check_count(count);
        if history.split {
            nbt.put_bool(SPLIT_TAG, true);
            return;
        }
        history.count = Some(count);
        insert_record(&mut history.records, self.storage_record(world, chunk));
        nbt.put_list(
            RECORDS_TAG,
            history
                .records
                .iter()
                .map(SavedItemRecord::to_nbt)
                .collect(),
        );
    }

    pub(crate) fn restore_saved_item_records(
        &self,
        world: &Path,
        chunk: Vector2<i32>,
        actor_tree: &NbtCompound,
    ) {
        let Some(actor) = find_actor(actor_tree, self.get_entity().entity_uuid) else {
            return;
        };
        let count = self
            .item_stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .item_count;
        let mut history = self
            .saved_records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if actor.get_bool(SPLIT_TAG) == Some(true) {
            history.mark_split();
            return;
        }
        history.count = Some(count);
        if let Some(records) = actor.get_list(RECORDS_TAG) {
            for record in records.iter().filter_map(SavedItemRecord::from_nbt) {
                insert_record(&mut history.records, record);
            }
        }
        // Older records have no history. Residency supplies the actual file position,
        // which can differ from the actor's position inside the record.
        insert_record(&mut history.records, self.storage_record(world, chunk));
    }

    pub(super) fn inherit_saved_item_records(
        &self,
        source: &Self,
        target_count: u8,
        source_count: u8,
    ) {
        let mut previous = std::mem::take(
            &mut *source
                .saved_records
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        previous.check_count(source_count);
        let mut history = self
            .saved_records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        history.check_count(target_count);
        if previous.split || history.split {
            history.mark_split();
        } else {
            for record in previous.records {
                insert_record(&mut history.records, record);
            }
            history.count = (!history.records.is_empty()).then_some(target_count + source_count);
        }
    }

    pub(super) fn mark_partial_item_transfer(&self) {
        self.saved_records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .mark_split();
    }
}

pub fn annotate_saved_item_records(
    nbt: &mut NbtCompound,
    items: &FxHashMap<Uuid, &ItemEntity>,
    world: &Path,
    chunk: Vector2<i32>,
) {
    if let Some(item) = nbt.get_uuid("UUID").and_then(|uuid| items.get(&uuid)) {
        item.write_saved_item_records(world, chunk, nbt);
    }
    if let Some(NbtTag::List(passengers)) = nbt.child_tags.get_mut("Passengers") {
        for passenger in passengers {
            if let NbtTag::Compound(passenger) = passenger {
                annotate_saved_item_records(passenger, items, world, chunk);
            }
        }
    }
}

fn insert_record(records: &mut Vec<SavedItemRecord>, record: SavedItemRecord) {
    if !records.contains(&record) {
        records.push(record);
    }
}

fn find_actor(nbt: &NbtCompound, uuid: Uuid) -> Option<&NbtCompound> {
    if nbt.get_uuid("UUID") == Some(uuid) {
        return Some(nbt);
    }
    nbt.get_list("Passengers")?
        .iter()
        .filter_map(NbtTag::extract_compound)
        .find_map(|passenger| find_actor(passenger, uuid))
}
