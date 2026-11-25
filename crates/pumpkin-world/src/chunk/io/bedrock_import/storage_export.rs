use std::{
    fs::{self, DirEntry},
    io::{Cursor, ErrorKind},
    path::Path,
};

use bytes::Bytes;
use pumpkin_data::item_stack::ItemStack;
use pumpkin_nbt::{
    Nbt,
    compound::NbtCompound,
    deserializer::{NbtReadHelper, NbtReadHelperBedrockDisk},
    tag::NbtTag,
};
use sculith_util::math::{position::BlockPos, vector2::Vector2};

use super::{
    AnvilLevelInfo, AnvilRegion, BLOCK_ENTITY, ChunkData, DB, Error, GeneratorPolicy, ImportStats,
    MAX_TABLE_BLOCK_BYTES, RegionFile, bedrock_db_options, copy_database, ensure_absent,
    index_chunks, invalid, new_destination, read_chunk, read_level, region_chunk_index,
    storage::{self, SLOTS, StorageItemRegistry},
    terrain::{DiskBlockStates, TerrainRegistry, export_column_blocks},
};
use crate::{
    chunk::{
        ChunkReadingError,
        format::{linear::LinearV2File, pump::PumpFile},
        io::converter::RegionFormat,
    },
    level::LevelFolder,
    world_info::WorldInfoReader,
};

#[derive(Debug)]
pub struct StorageExportStats {
    pub containers_checked: usize,
    pub inventories_updated: usize,
    pub preserved: ImportStats,
}

#[derive(Debug)]
pub struct WorldEditsExportStats {
    pub storage: StorageExportStats,
    pub block_columns_updated: usize,
}

/// Copies a stopped Bedrock world and updates the inventories of its supported containers
/// from the corresponding stopped Sculith world. The new copy is published after validation.
pub fn export_storage(
    bedrock_source: &Path,
    sculith_source: &Path,
    destination: &Path,
) -> Result<StorageExportStats, Error> {
    export_world(bedrock_source, sculith_source, destination, false).map(|report| report.storage)
}

/// Also copies supported block edits within existing native columns. Preserves the source
/// biomes, entities and metadata, and publishes the destination only after validation.
pub fn export_world_edits(
    bedrock_source: &Path,
    sculith_source: &Path,
    destination: &Path,
) -> Result<WorldEditsExportStats, Error> {
    export_world(bedrock_source, sculith_source, destination, true)
}

fn export_world(
    bedrock_source: &Path,
    sculith_source: &Path,
    destination: &Path,
    include_block_edits: bool,
) -> Result<WorldEditsExportStats, Error> {
    let template = bedrock_source.canonicalize()?;
    let sculith = sculith_source.canonicalize()?;
    crate::data::checkpoint::SaveCheckpoint::require_materialized(&sculith)?;
    let destination = new_destination(destination, &template)?;
    if destination.starts_with(&sculith) {
        return Err(invalid("destination must be outside the Sculith world"));
    }
    let terrain = TerrainRegistry::load()?;
    let block_states = include_block_edits
        .then(|| DiskBlockStates::load(&terrain))
        .transpose()?;
    let items = storage::read_import_registry(&sculith)?;
    let (template_info, _) = read_level(&template, &terrain, GeneratorPolicy::NormalIfUnspecified)?;
    let sculith_info = AnvilLevelInfo.read_world_info(&sculith)?;
    if template_info.world_gen_settings.seed != sculith_info.world_gen_settings.seed {
        return Err(invalid("the Bedrock and Sculith world seeds differ"));
    }

    let staging = tempfile::tempdir_in(
        destination
            .parent()
            .ok_or_else(|| invalid("invalid destination"))?,
    )?;
    let output = staging.path().join("world");
    fs::create_dir(&output)?;
    for entry in fs::read_dir(&template)? {
        let entry = entry?;
        if entry.file_name() != "db" {
            copy_world_entry(&entry, &output)?;
        }
    }
    let database = output.join("db");
    copy_database(&template.join("db"), &database)?;
    let db = DB::open(database, bedrock_db_options())?;
    let report = export_columns(&db, &sculith, &terrain, &items, block_states.as_ref())?;
    if !include_block_edits && report.storage.containers_checked == 0 {
        return Err(invalid(
            "the Bedrock world contains no supported storage containers",
        ));
    }
    db.close()?;
    ensure_absent(&destination)?;
    fs::rename(output, destination)?;
    Ok(report)
}

fn export_columns(
    db: &DB,
    sculith: &Path,
    terrain: &TerrainRegistry,
    items: &StorageItemRegistry,
    block_states: Option<&DiskBlockStates>,
) -> Result<WorldEditsExportStats, Error> {
    let mut preserved = ImportStats::default();
    let regions = index_chunks(db, &mut preserved, None, Err)?;
    let mut inventories_updated = 0;
    let mut block_columns_updated = 0;
    for ((dimension, rx, rz), columns) in regions {
        let region_pos = Vector2::new(rx, rz);
        let folders = LevelFolder::for_dimension(sculith.to_path_buf(), &dimension.target());
        let mut region = None;
        for ((x, z), sections) in columns {
            let pos = Vector2::new(x, z);
            let mut key = [0; 13];
            let tag = dimension.write_column_prefix(pos, &mut key);
            key[tag] = BLOCK_ENTITY;
            let key = &key[..=tag];
            let record = db.try_get(key)?;
            if record.is_none() && block_states.is_none() {
                continue;
            }
            let original = read_chunk(db, dimension, pos, sections, terrain, items, &mut preserved)
                .map_err(|error| dimension.chunk_error(pos, error))?;
            let has_storage = !original
                .pending_block_entities
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty();
            if !has_storage && block_states.is_none() {
                continue;
            }
            let shared = read_storage_chunk(&mut region, &folders.region_folder, region_pos, pos)
                .map_err(|error| dimension.chunk_error(pos, error))?;
            if let Some(states) = block_states {
                let edited = export_column_blocks(db, dimension, &original, &shared, states)
                    .map_err(|error| dimension.chunk_error(pos, error))?;
                block_columns_updated += usize::from(edited);
            }
            if has_storage && let Some(record) = record {
                let (updated, edited) = export_record(&record, &original, &shared, terrain, items)
                    .map_err(|error| dimension.chunk_error(pos, error))?;
                if edited != 0 {
                    db.put(key, &updated)?;
                    inventories_updated += edited;
                }
            }
        }
    }
    Ok(WorldEditsExportStats {
        block_columns_updated,
        storage: StorageExportStats {
            containers_checked: preserved.containers_imported,
            inventories_updated,
            preserved,
        },
    })
}

enum StorageRegion {
    Anvil(AnvilRegion),
    Linear(LinearV2File),
    Pump(PumpFile),
}

impl StorageRegion {
    fn open(folder: &Path, region: Vector2<i32>) -> Result<Self, Error> {
        let mut source = None;
        for format in [
            RegionFormat::Anvil,
            RegionFormat::Linear,
            RegionFormat::Pump,
        ] {
            let path = folder.join(format.file_name(region));
            match fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => {
                    if source.replace((format, path)).is_some() {
                        return Err(invalid("multiple Sculith formats for the matching region"));
                    }
                }
                Ok(_) => return Err(invalid(format!("not a region file: {}", path.display()))),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let (format, path) = source.ok_or_else(|| invalid("missing matching Sculith chunk"))?;
        Ok(match format {
            RegionFormat::Anvil => Self::Anvil(AnvilRegion::open_read_only(&path, region)?),
            RegionFormat::Linear => Self::Linear(LinearV2File::open(&path, region)?),
            RegionFormat::Pump => Self::Pump(PumpFile::open_read_only(&path, region)?),
        })
    }

    fn read_chunk(&self, pos: Vector2<i32>) -> Result<ChunkData, Error> {
        let index = region_chunk_index(pos);
        let bytes = match self {
            Self::Anvil(region) => decode_storage_chunk(region, index),
            Self::Linear(region) => decode_storage_chunk(region, index),
            Self::Pump(region) => decode_storage_chunk(region, index),
        }?;
        ChunkData::internal_from_bytes(&bytes, pos)
            .map_err(|error| Error::Read(ChunkReadingError::ParsingError(error)))
    }
}

fn decode_storage_chunk<R: RegionFile>(region: &R, index: usize) -> Result<Bytes, Error> {
    let raw = region
        .read_chunk(index)?
        .ok_or_else(|| invalid("missing matching Sculith chunk"))?;
    Ok(R::decode_chunk(raw)?)
}

fn read_storage_chunk(
    region: &mut Option<StorageRegion>,
    folder: &Path,
    region_pos: Vector2<i32>,
    pos: Vector2<i32>,
) -> Result<ChunkData, Error> {
    let region = match region {
        Some(region) => region,
        pending => pending.insert(StorageRegion::open(folder, region_pos)?),
    };
    region.read_chunk(pos)
}

fn copy_world_entry(entry: &DirEntry, output: &Path) -> Result<(), Error> {
    let destination = output.join(entry.file_name());
    let kind = entry.file_type()?;
    if kind.is_dir() {
        fs::create_dir(&destination)?;
        for child in fs::read_dir(entry.path())? {
            copy_world_entry(&child?, &destination)?;
        }
    } else if kind.is_file() {
        fs::copy(entry.path(), destination)?;
    } else {
        return Err(invalid("the Bedrock world contains a non-regular file"));
    }
    Ok(())
}

pub(super) fn export_record(
    raw: &[u8],
    original: &ChunkData,
    shared: &ChunkData,
    terrain: &TerrainRegistry,
    items: &StorageItemRegistry,
) -> Result<(Vec<u8>, usize), Error> {
    let baseline = original
        .pending_block_entities
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entities = shared
        .pending_block_entities
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut reader = NbtReadHelperBedrockDisk::new(Cursor::new(raw));
    let mut output = Vec::with_capacity(raw.len());
    let mut edited = 0;
    let mut offset = 0;
    while offset < raw.len() {
        let mut saved = Nbt::read(&mut reader)?;
        let end = reader.reader().position() as usize;
        let origin = storage::position(&saved)
            .and_then(|pos| baseline.get(&pos).map(|entity| (pos, entity)));
        if let Some((position, previous)) = origin {
            let current = entities
                .get(&position)
                .ok_or_else(|| invalid(format!("missing Sculith container at {position}")))?;
            let changed = replace_slots(
                &mut saved.root_tag,
                previous,
                current,
                shared,
                position,
                terrain,
                items,
            )
            .map_err(|reason| invalid(format!("Sculith container at {position} ({reason})")))?;
            if changed {
                saved.write_to_writer_bedrock_disk(&mut output)?;
                edited += 1;
                offset = end;
                continue;
            }
        }
        // Preserve unsupported and unchanged compounds, including their original tag encoding.
        output.extend_from_slice(&raw[offset..end]);
        offset = end;
    }
    if output.len() > MAX_TABLE_BLOCK_BYTES {
        return Err(invalid("oversized exported Bedrock block-entity record"));
    }
    Ok((output, edited))
}

fn replace_slots(
    saved: &mut NbtCompound,
    previous: &NbtCompound,
    current: &NbtCompound,
    shared: &ChunkData,
    position: BlockPos,
    terrain: &TerrainRegistry,
    items: &StorageItemRegistry,
) -> Result<bool, String> {
    let kind = saved
        .get_string("id")
        .ok_or("missing Bedrock container type")?;
    storage::check_container_at(shared, kind, position)?;
    if current.get_string("id") != previous.get_string("id") {
        return Err("container type changed".into());
    }
    if let Some(field) = current
        .child_tags
        .iter()
        .filter_map(|(field, value)| match field.as_ref() {
            "id" | "x" | "y" | "z" | "Items" => None,
            "keepPacked" if matches!(value, NbtTag::Byte(0)) => None,
            _ => Some(field),
        })
        .min()
    {
        return Err(format!("unsupported Sculith container field {field:?}"));
    }
    let before = java_slots(previous, items)?;
    let after = java_slots(current, items)?;
    if before
        .iter()
        .zip(&after)
        .all(|(before, after)| match (before, after) {
            (Some(before), Some(after)) => before.are_equal(after),
            (None, None) => true,
            _ => false,
        })
    {
        return Ok(false);
    }
    let existing = saved.get_list("Items").unwrap_or_default();
    let mut encoded = Vec::new();
    for (slot, stack) in after.into_iter().enumerate() {
        let Some(stack) = stack else { continue };
        let prototype = existing
            .iter()
            .filter_map(NbtTag::extract_compound)
            .filter_map(|entry| {
                let index = usize::try_from(entry.get_byte("Slot")?).ok()?;
                let old = before.get(index)?.as_ref()?;
                old.are_items_and_components_equal(&stack)
                    .then_some((entry, index))
            })
            .min_by_key(|&(_, index)| index != slot)
            .map(|(entry, _)| entry);
        let mut entry = match prototype {
            Some(prototype) => prototype.clone(),
            None => storage::new_bedrock_stack(&stack, items)?,
        };
        entry.put_byte("Slot", slot as i8);
        entry.put_byte("Count", stack.item_count as i8);
        let decoded = storage::convert_stack(&entry, terrain, items)?;
        if !decoded.are_equal(&stack) {
            return Err(format!("slot {slot} has no reversible Bedrock mapping"));
        }
        encoded.push(NbtTag::Compound(entry));
    }
    saved.put_list("Items", encoded);
    Ok(true)
}

fn java_slots(
    entity: &NbtCompound,
    items: &StorageItemRegistry,
) -> Result<[Option<ItemStack>; SLOTS], String> {
    let entries = match entity.get("Items") {
        None => &[][..],
        Some(NbtTag::List(items)) => items.as_slice(),
        Some(_) => return Err("invalid Sculith Items list".into()),
    };
    let mut slots = [const { None }; SLOTS];
    for entry in entries {
        let entry = entry
            .extract_compound()
            .ok_or("invalid item in Sculith Items list")?;
        let slot = entry
            .get_byte("Slot")
            .and_then(|slot| usize::try_from(slot).ok())
            .filter(|&slot| slot < SLOTS)
            .ok_or("invalid Sculith item slot")?;
        let stack = storage::read_sculith_stack(entry, items)
            .map_err(|error| format!("slot {slot}: {error}"))?;
        if slots[slot].replace(stack).is_some() {
            return Err(format!("duplicate Sculith item slot {slot}"));
        }
    }
    Ok(slots)
}
