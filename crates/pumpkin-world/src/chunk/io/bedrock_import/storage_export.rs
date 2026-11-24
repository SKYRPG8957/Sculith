use std::{
    fs::{self, DirEntry},
    io::Cursor,
    path::Path,
};

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
    terrain::TerrainRegistry,
};
use crate::{chunk::ChunkReadingError, world_info::WorldInfoReader};

#[derive(Debug)]
pub struct StorageExportStats {
    pub containers_checked: usize,
    pub inventories_updated: usize,
    pub preserved: ImportStats,
}

/// Copies a stopped Bedrock world and updates the inventories of its supported containers
/// from the corresponding stopped Sculith world. The new copy is published after validation.
pub fn export_storage(
    bedrock_source: &Path,
    sculith_source: &Path,
    destination: &Path,
) -> Result<StorageExportStats, Error> {
    let template = bedrock_source.canonicalize()?;
    let sculith = sculith_source.canonicalize()?;
    crate::data::checkpoint::SaveCheckpoint::require_materialized(&sculith)?;
    let destination = new_destination(destination, &template)?;
    if destination.starts_with(&sculith) {
        return Err(invalid("destination must be outside the Sculith world"));
    }
    let terrain = TerrainRegistry::load()?;
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
    let mut preserved = ImportStats::default();
    let regions = index_chunks(&db, &mut preserved, Err)?;
    let mut inventories_updated = 0;
    for ((rx, rz), columns) in regions {
        let region_pos = Vector2::new(rx, rz);
        let path = sculith
            .join("region")
            .join(AnvilRegion::file_name(region_pos));
        let mut region = None;
        for ((x, z), sections) in columns {
            let mut key = [0; 9];
            key[..4].copy_from_slice(&x.to_le_bytes());
            key[4..8].copy_from_slice(&z.to_le_bytes());
            key[8] = BLOCK_ENTITY;
            let Some(record) = db.try_get(&key)? else {
                continue;
            };
            let original = read_chunk(&db, x, z, sections, &terrain, &items, &mut preserved)
                .map_err(|error| error.at_chunk(x, z))?;
            if original
                .pending_block_entities
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
            {
                continue;
            }
            let region = match &mut region {
                Some(region) => region,
                pending => pending.insert(AnvilRegion::open_read_only(&path, region_pos)?),
            };
            let pos = Vector2::new(x, z);
            let raw = region
                .read_chunk(region_chunk_index(pos))?
                .ok_or_else(|| invalid("missing matching Sculith chunk").at_chunk(x, z))?;
            let bytes = AnvilRegion::decode_chunk(raw)?;
            let shared = ChunkData::internal_from_bytes(&bytes, pos).map_err(|error| {
                Error::Read(ChunkReadingError::ParsingError(error)).at_chunk(x, z)
            })?;
            let (updated, edited) = export_record(&record, &original, &shared, &terrain, &items)
                .map_err(|error| error.at_chunk(x, z))?;
            if edited != 0 {
                db.put(&key, &updated)?;
                inventories_updated += edited;
            }
        }
    }
    if preserved.containers_imported == 0 {
        return Err(invalid(
            "the Bedrock world contains no supported storage containers",
        ));
    }
    db.close()?;
    ensure_absent(&destination)?;
    fs::rename(output, destination)?;
    Ok(StorageExportStats {
        containers_checked: preserved.containers_imported,
        inventories_updated,
        preserved,
    })
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
            None => storage::new_bedrock_stack(&stack)?,
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
