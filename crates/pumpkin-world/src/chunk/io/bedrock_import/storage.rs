use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
};

use pumpkin_data::{
    Block,
    block_properties::{ChestLikeProperties, ChestType},
    data_component::DataComponent,
    item::{BedrockItem, Item, JavaToBedrockItemMapping},
    item_stack::ItemStack,
};
use pumpkin_nbt::{
    Nbt,
    compound::NbtCompound,
    deserializer::{NbtReadHelper, NbtReadHelperBedrockDisk},
    tag::NbtTag,
};
use pumpkin_util::math::position::BlockPos;

use super::{
    Error, ImportStats, MAX_TABLE_BLOCK_BYTES, OVERWORLD, invalid, terrain::TerrainRegistry,
};
use crate::chunk::ChunkData;

pub(super) const SLOTS: usize = 27;

pub(super) type StorageItemRegistry = HashMap<&'static str, i16>;

pub(super) fn item_registry() -> StorageItemRegistry {
    BedrockItem::ALL_BEDROCK_ITEMS
        .iter()
        .map(|item| (item.registry_key, item.id))
        .collect()
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
        let converted = match (kind, position) {
            ("Chest" | "Barrel", Some(position)) => {
                convert_container(&saved, kind, position, chunk, terrain, items)
                    .map(|entity| (position, entity))
            }
            ("Chest" | "Barrel", None) => Err("invalid container coordinates".into()),
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
    let BlockPos(pos) = position;
    if (pos.x.div_euclid(16), pos.z.div_euclid(16)) != (chunk.x, chunk.z) {
        return Err("container coordinates are outside its chunk".into());
    }
    if !(OVERWORLD.min_y..OVERWORLD.min_y + OVERWORLD.height).contains(&pos.y) {
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
    let expected = if kind == "Chest" {
        &Block::CHEST
    } else {
        &Block::BARREL
    };
    if Block::from_state_id(state).id != expected.id {
        return Err("block state does not match the container type".into());
    }
    if kind == "Chest" && ChestLikeProperties::from_state_id(state).r#type != ChestType::Single {
        return Err("only single chests are supported".into());
    }
    Ok(())
}

fn convert_container(
    saved: &NbtCompound,
    kind: &str,
    position: BlockPos,
    chunk: &ChunkData,
    terrain: &TerrainRegistry,
    items: &StorageItemRegistry,
) -> Result<NbtCompound, String> {
    check_container_at(chunk, kind, position)?;
    let BlockPos(pos) = position;
    if let Some(field) = saved
        .child_tags
        .keys()
        .filter(|field| {
            !matches!(
                field.as_ref(),
                "id" | "x" | "y" | "z" | "Items" | "BlockEntityVersion" | "Findable" | "isMovable"
            )
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
    let mut converted = Vec::with_capacity(source_items.len().min(SLOTS));
    for item in source_items {
        let source = item
            .extract_compound()
            .ok_or("invalid item in Items list")?;
        let slot = source
            .get_byte("Slot")
            .and_then(|slot| usize::try_from(slot).ok())
            .filter(|&slot| slot < SLOTS)
            .ok_or("invalid item slot")?;
        if std::mem::replace(&mut used[slot], true) {
            return Err(format!("duplicate slot {slot}"));
        }
        let stack = convert_stack(source, terrain, items)
            .map_err(|error| format!("slot {slot}: {error}"))?;
        let mut entry = NbtCompound::new();
        entry.put_byte("Slot", slot as i8);
        stack.write_item_stack(&mut entry);
        converted.push(NbtTag::Compound(entry));
    }
    let mut entity = NbtCompound::new();
    entity.put_string(
        "id",
        (if kind == "Chest" {
            "minecraft:chest"
        } else {
            "minecraft:barrel"
        })
        .to_owned(),
    );
    entity.put_int("x", pos.x);
    entity.put_int("y", pos.y);
    entity.put_int("z", pos.z);
    if !converted.is_empty() {
        entity.put_list("Items", converted);
    }
    Ok(entity)
}

pub(super) fn convert_stack(
    source: &NbtCompound,
    terrain: &TerrainRegistry,
    items: &StorageItemRegistry,
) -> Result<ItemStack, String> {
    if let Some(field) = source
        .child_tags
        .iter()
        .filter(|(field, tag)| match field.as_ref() {
            "Slot" | "Name" | "Count" | "WasPickedUp" | "Block" => false,
            "Damage" => !matches!(tag, NbtTag::Short(0)),
            "tag" => !matches!(tag, NbtTag::Compound(data) if data.is_empty()),
            _ => true,
        })
        .map(|(field, _)| field)
        .min()
    {
        return Err(format!("unsupported item field {field:?}"));
    }
    let name = source.get_string("Name").ok_or("missing item name")?;
    let item = items
        .get(name)
        .and_then(|&id| JavaToBedrockItemMapping::from_bedrock(id, 0))
        .map(|mapping| mapping.java_item)
        .filter(|item| *item != &Item::AIR)
        .ok_or_else(|| format!("item {name:?} has no plain mapping"))?;
    // Bedrock potion metadata zero means water, while Java's default has no potion.
    if item
        .components
        .iter()
        .any(|(component, _)| *component == DataComponent::PotionContents)
    {
        return Err(format!("item {name:?} requires potion data conversion"));
    }
    if let Some(block) = source.get("Block") {
        let block = block.extract_compound().ok_or("invalid block item data")?;
        let state = terrain
            .block(block)
            .map_err(|error| format!("unsupported block item state: {error}"))?;
        let block = Block::from_state_id(state);
        if block.name != item.registry_key || state != block.default_state.id {
            return Err("only default states for block items are supported".into());
        }
    }
    let count = source
        .get_byte("Count")
        .and_then(|count| u8::try_from(count).ok())
        .filter(|&count| count > 0)
        .ok_or("invalid item count")?;
    let stack = ItemStack::new(count, item);
    if count > stack.get_max_stack_size() {
        return Err(format!("item count {count} exceeds the stack limit"));
    }
    Ok(stack)
}
