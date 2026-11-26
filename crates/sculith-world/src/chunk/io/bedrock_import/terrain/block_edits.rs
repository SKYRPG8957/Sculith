use std::io::Cursor;

use pumpkin_data::{BlockState, BlockStateId};
use pumpkin_nbt::{
    Nbt,
    deserializer::{NbtReadHelper, NbtReadHelperBedrockDisk},
};
use sculith_util::math::vector2::Vector2;

use crate::chunk::{ChunkData, palette::BlockPalette};

use super::super::{BIOMES, DB, Error, SUBCHUNK, SavedDimension, invalid};
use super::{
    bedrock_index,
    disk_write::{DiskBlockStates, rewrite_subchunk},
};

const PENDING_TICKS: u8 = 0x33;
type SectionChanges = Vec<(usize, BlockStateId)>;

struct ColumnChanges {
    sections: Vec<SectionChanges>,
    changes_height: [bool; 256],
}

impl ColumnChanges {
    fn collect(
        original: &[BlockPalette],
        shared: &[BlockPalette],
        disk_sections: usize,
    ) -> Result<Self, Error> {
        if original.len() != shared.len() || shared.len() < disk_sections {
            return Err(invalid("Sculith and Bedrock chunk heights differ"));
        }
        let mut changes = Self {
            sections: vec![Vec::new(); disk_sections],
            changes_height: [false; 256],
        };
        for (section, (old, new)) in original.iter().zip(shared).enumerate() {
            for x in 0..16 {
                for z in 0..16 {
                    for y in 0..16 {
                        let old = old.get(x, y, z);
                        let new = new.get(x, y, z);
                        if old != new {
                            changes.add(section, (x, y, z), old, new)?;
                        }
                    }
                }
            }
        }
        Ok(changes)
    }

    fn add(
        &mut self,
        section: usize,
        (x, y, z): (usize, usize, usize),
        old: BlockStateId,
        new: BlockStateId,
    ) -> Result<(), Error> {
        let section = self
            .sections
            .get_mut(section)
            .ok_or_else(|| invalid("block edits outside the native dimension height"))?;
        let old_block = old.to_state();
        let new_block = new.to_state();
        if !plain_edit(old_block) || !plain_edit(new_block) {
            return Err(invalid(format!(
                "unsupported terrain edit from {} to {}",
                old.to_block().name,
                new.to_block().name
            )));
        }
        self.changes_height[z * 16 + x] |= old_block.opacity != new_block.opacity;
        section.push((bedrock_index(x, y, z), new));
        Ok(())
    }

    fn fix_heights(
        &self,
        data: &mut [u8],
        original: &[BlockPalette],
        shared: &[BlockPalette],
    ) -> Result<bool, Error> {
        let mut changed = false;
        for (column, &affects_height) in self.changes_height.iter().enumerate() {
            if !affects_height {
                continue;
            }
            let (x, z) = (column % 16, column / 16);
            let before = native_height(original, x, z)?;
            let after = native_height(shared, x, z)?;
            if before != after {
                let offset = column * 2;
                let entry = data
                    .get_mut(offset..offset + 2)
                    .ok_or_else(|| invalid("truncated Bedrock height map"))?;
                if entry != before.to_le_bytes() {
                    return Err(invalid(format!(
                        "unrecognized Bedrock height at local column {x}, {z}"
                    )));
                }
                entry.copy_from_slice(&after.to_le_bytes());
                changed = true;
            }
        }
        Ok(changed)
    }
}

const fn plain_edit(block: &BlockState) -> bool {
    block.block_entity_type == u16::MAX
        && matches!(block.opacity, 0 | 15)
        && !block.is_waterlogged()
        && !block.is_liquid()
}

fn native_height(palettes: &[BlockPalette], x: usize, z: usize) -> Result<u16, Error> {
    for (index, palette) in palettes.iter().enumerate().rev() {
        for y in (0..16).rev() {
            match palette.get(x, y, z).to_state().opacity {
                0 => {}
                15 => return Ok((index * 16 + y + 1) as u16),
                _ => return Err(invalid(format!("partial opacity at local column {x}, {z}"))),
            }
        }
    }
    Ok(0)
}

fn native_has_pending_ticks(db: &DB, key: &[u8]) -> Result<bool, Error> {
    let Some(value) = db.try_get(key)? else {
        return Ok(false);
    };
    let mut reader = NbtReadHelperBedrockDisk::new(Cursor::new(value.as_slice()));
    let nbt = Nbt::read(&mut reader)?;
    if reader.reader().position() != value.len() as u64 {
        return Err(invalid("unrecognized Bedrock scheduled ticks"));
    }
    Ok(nbt
        .get_list("tickList")
        .is_none_or(|entries| !entries.is_empty()))
}

pub(in crate::chunk::io::bedrock_import) fn export_column_blocks(
    db: &DB,
    dimension: SavedDimension,
    original: &ChunkData,
    shared: &ChunkData,
    states: &DiskBlockStates,
) -> Result<bool, Error> {
    let old = original
        .section
        .block_sections
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let new = shared
        .section
        .block_sections
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if original.section.min_y != shared.section.min_y {
        return Err(invalid("Sculith and Bedrock chunk origins differ"));
    }
    let disk_sections = dimension.disk_sections();
    let edits = ColumnChanges::collect(&old, &new, disk_sections)?;
    if edits.sections.iter().all(Vec::is_empty) {
        return Ok(false);
    }

    let mut key = [0; 14];
    let tag = dimension.write_column_prefix(Vector2::new(original.x, original.z), &mut key);
    key[tag] = PENDING_TICKS;
    if native_has_pending_ticks(db, &key[..=tag])?
        || shared.block_ticks.has_ticks()
        || shared.fluid_ticks.has_ticks()
    {
        return Err(invalid(
            "edited column contains pending block or fluid ticks",
        ));
    }

    key[tag] = BIOMES;
    let mut biomes = db
        .try_get(&key[..=tag])?
        .ok_or_else(|| invalid("missing biome record"))?;
    if edits.fix_heights(&mut biomes, &old[..disk_sections], &new[..disk_sections])? {
        db.put(&key[..=tag], &biomes)?;
    }
    key[tag] = SUBCHUNK;
    for (index, changes) in edits
        .sections
        .iter()
        .enumerate()
        .filter(|(_, items)| !items.is_empty())
    {
        let y = (original.section.min_y / 16 + index as i32) as i8;
        key[tag + 1] = y.cast_unsigned();
        let section_key = &key[..tag + 2];
        let old = db.try_get(section_key)?;
        let updated = rewrite_subchunk(old.as_deref(), y, changes, states)?;
        db.put(section_key, &updated)?;
    }
    Ok(true)
}
