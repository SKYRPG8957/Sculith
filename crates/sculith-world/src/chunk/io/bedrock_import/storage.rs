use std::{collections::HashSet, io::Cursor};

use pumpkin_data::{
    Block,
    block_properties::{ChestLikeProperties, ChestType},
    recipes::CookingRecipeKind,
};
use pumpkin_nbt::{
    Nbt,
    compound::NbtCompound,
    deserializer::{NbtReadHelper, NbtReadHelperBedrockDisk},
    tag::NbtTag,
};
use sculith_util::math::position::BlockPos;

use super::{Error, ImportStats, MAX_TABLE_BLOCK_BYTES, invalid, terrain::TerrainRegistry};
use crate::chunk::ChunkData;

mod cooking;
mod items;
pub(super) use items::{
    StorageItemRegistry, convert_stack, item_registry, new_bedrock_stack, read_import_registry,
    read_sculith_stack, write_item_version,
};

pub(super) const SLOTS: usize = 27;

struct SavedContainer {
    block: &'static Block,
    id: &'static str,
    slots: usize,
    cooking: Option<CookingRecipeKind>,
}

impl SavedContainer {
    fn from_bedrock_name(name: &str) -> Option<Self> {
        let (block, id, cooking) = match name {
            "Chest" => (&Block::CHEST, "minecraft:chest", None),
            "Barrel" => (&Block::BARREL, "minecraft:barrel", None),
            "Furnace" => (
                &Block::FURNACE,
                "minecraft:furnace",
                Some(CookingRecipeKind::Smelting),
            ),
            "BlastFurnace" => (
                &Block::BLAST_FURNACE,
                "minecraft:blast_furnace",
                Some(CookingRecipeKind::Blasting),
            ),
            "Smoker" => (
                &Block::SMOKER,
                "minecraft:smoker",
                Some(CookingRecipeKind::Smoking),
            ),
            _ => return None,
        };
        let slots = if cooking.is_some() { 3 } else { SLOTS };
        Some(Self {
            block,
            id,
            slots,
            cooking,
        })
    }
}

pub(super) fn import_record(
    bytes: &[u8],
    chunk: &mut ChunkData,
    terrain: &TerrainRegistry,
    items: &StorageItemRegistry,
    stats: &mut ImportStats,
) -> Result<bool, Error> {
    if bytes.len() > MAX_TABLE_BLOCK_BYTES {
        return Err(invalid("oversized Bedrock block-entity record"));
    }
    let mut reader = NbtReadHelperBedrockDisk::new(Cursor::new(bytes));
    let mut positions = HashSet::new();
    let mut complete = true;
    while reader.reader().position() < bytes.len() as u64 {
        let saved = Nbt::read(&mut reader)?.root_tag;
        let kind = saved.get_string("id").unwrap_or("<missing id>");
        let position = position(&saved);
        if let Some(position) = position
            && !positions.insert(position)
        {
            return Err(invalid(format!("duplicate block entity at {position}")));
        }
        let container = SavedContainer::from_bedrock_name(kind)
            .filter(|container| container.cooking.is_none() || items.import_cooking_appliances);
        let converted = match (container, position) {
            (Some(container), Some(position)) => {
                convert_container(&saved, &container, position, chunk, terrain, items)
                    .map(|entity| (position, entity))
            }
            (Some(_), None) => Err("invalid container coordinates".into()),
            _ => Err("unsupported block entity type".into()),
        };
        match converted {
            Ok((position, entity)) => {
                chunk
                    .pending_block_entities
                    .get_mut()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(position, entity);
                stats.containers_imported += 1;
            }
            Err(reason) => {
                stats.skip_block_entity(kind, position, reason);
                complete = false;
            }
        }
    }
    Ok(complete)
}

pub(super) fn position(saved: &NbtCompound) -> Option<BlockPos> {
    Some(BlockPos::new(
        saved.get_int("x")?,
        saved.get_int("y")?,
        saved.get_int("z")?,
    ))
}

pub(super) fn check_container_at(
    chunk: &ChunkData,
    kind: &str,
    position: BlockPos,
) -> Result<(), String> {
    let container =
        SavedContainer::from_bedrock_name(kind).ok_or("unsupported block entity type")?;
    check_container_block(chunk, container.block, position)
}

fn check_container_block(
    chunk: &ChunkData,
    expected: &'static Block,
    position: BlockPos,
) -> Result<(), String> {
    let BlockPos(pos) = position;
    if (pos.x.div_euclid(16), pos.z.div_euclid(16)) != (chunk.x, chunk.z) {
        return Err("container coordinates are outside its chunk".into());
    }
    let min_y = chunk.section.min_y;
    let max_y = min_y + 16 * chunk.section.count as i32;
    if !(min_y..max_y).contains(&pos.y) {
        return Err("container height is outside the world".into());
    }
    let state = chunk
        .section
        .get_block_absolute_y(
            pos.x.rem_euclid(16) as usize,
            pos.y,
            pos.z.rem_euclid(16) as usize,
        )
        .ok_or("container height is outside the world")?;
    if Block::from_state_id(state).id != expected.id {
        return Err("block state does not match the container type".into());
    }
    if expected.id == Block::CHEST.id
        && ChestLikeProperties::from_state_id(state).r#type != ChestType::Single
    {
        return Err("only single chests are supported".into());
    }
    Ok(())
}

fn convert_container(
    saved: &NbtCompound,
    container: &SavedContainer,
    position: BlockPos,
    chunk: &ChunkData,
    terrain: &TerrainRegistry,
    items: &StorageItemRegistry,
) -> Result<NbtCompound, String> {
    check_container_block(chunk, container.block, position)?;
    let BlockPos(pos) = position;
    if let Some(field) = saved
        .child_tags
        .keys()
        .filter(|field| match field.as_ref() {
            "id" | "x" | "y" | "z" | "Items" | "BlockEntityVersion" | "Findable" | "isMovable" => {
                false
            }
            "CustomName" => !items.supports_named_containers(),
            "BurnDuration" | "BurnTime" | "CookTime" | "StoredXPInt" => container.cooking.is_none(),
            _ => true,
        })
        .min()
    {
        return Err(format!("unsupported container field {field:?}"));
    }
    if !matches!(
        saved.get("BlockEntityVersion"),
        None | Some(NbtTag::Byte(0) | NbtTag::Short(0) | NbtTag::Int(0))
    ) {
        return Err("unsupported block entity version".into());
    }
    let source_items = match saved.get("Items") {
        None => &[][..],
        Some(NbtTag::List(items)) => items.as_slice(),
        Some(_) => return Err("invalid Items list".into()),
    };
    let mut used = [false; SLOTS];
    let mut converted = Vec::with_capacity(source_items.len().min(container.slots));
    let mut cooking_input = None;
    for item in source_items {
        let source = item
            .extract_compound()
            .ok_or("invalid item in Items list")?;
        let slot = source
            .get_byte("Slot")
            .and_then(|slot| usize::try_from(slot).ok())
            .filter(|&slot| slot < container.slots)
            .ok_or("invalid item slot")?;
        if std::mem::replace(&mut used[slot], true) {
            return Err(format!("duplicate slot {slot}"));
        }
        let stack = convert_stack(source, terrain, items)
            .map_err(|error| format!("slot {slot}: {error}"))?;
        if slot == 0 {
            cooking_input = Some(stack.item);
        }
        let mut entry = NbtCompound::new();
        entry.put_byte("Slot", slot as i8);
        stack.write_item_stack(&mut entry);
        converted.push(NbtTag::Compound(entry));
    }
    let mut entity = NbtCompound::new();
    entity.put_string("id", container.id.to_owned());
    entity.put_int("x", pos.x);
    entity.put_int("y", pos.y);
    entity.put_int("z", pos.z);
    if let Some(name) = saved.get("CustomName") {
        let name = name.extract_string().ok_or("invalid container name")?;
        if u16::try_from(pumpkin_nbt::cesu8::to_java_cesu8(name).len()).is_err() {
            return Err("container name is too long for Java NBT".into());
        }
        entity.put_string("CustomName", name.to_owned());
    }
    if !converted.is_empty() {
        entity.put_list("Items", converted);
    }
    if let Some(kind) = &container.cooking {
        cooking::write_metadata(saved, &mut entity, kind, cooking_input)?;
    }
    Ok(entity)
}
