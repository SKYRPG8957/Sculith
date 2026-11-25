use std::{collections::HashMap, io::Cursor};

use pumpkin_data::{BlockState, BlockStateId};
use pumpkin_nbt::{
    Nbt, NbtCompound,
    deserializer::{NbtReadHelper, NbtReadHelperBedrock, NbtReadHelperBedrockDisk},
};

use crate::chunk::palette::{BlockPalette, bedrock_palette_bits};

use super::{
    BLOCK_STATES, Error, SUBCHUNK_VERSION, TerrainRegistry, invalid, read_indices, read_palette_len,
};

type DiskReader<'a> = NbtReadHelperBedrockDisk<Cursor<&'a [u8]>>;

struct DiskState {
    preference: (bool, bool),
    bytes: Vec<u8>,
}

pub(in crate::chunk::io::bedrock_import) struct DiskBlockStates(HashMap<BlockStateId, DiskState>);

impl DiskBlockStates {
    pub(in crate::chunk::io::bedrock_import) fn load(
        registry: &TerrainRegistry,
    ) -> Result<Self, Error> {
        let mut states = HashMap::<BlockStateId, DiskState>::new();
        let mut reader = NbtReadHelperBedrock::new(Cursor::new(BLOCK_STATES));
        while reader.reader().position() < BLOCK_STATES.len() as u64 {
            let entry = Nbt::read(&mut reader)?;
            let Ok(state) = registry.block(&entry) else {
                continue;
            };
            let name = entry
                .get_string("name")
                .ok_or_else(|| invalid("missing block name"))?;
            let exact_name = name.strip_prefix("minecraft:") == Some(state.to_block().name);
            // An imported alias can preserve its original NBT. New cells use
            // the canonical network state when the registry provides one.
            let canonical_state = entry
                .get_int("network_id")
                .is_some_and(|id| id.cast_unsigned() == BlockState::to_be_network_id(state));
            let preference = (canonical_state, exact_name);
            if states
                .get(&state)
                .is_some_and(|current| current.preference >= preference)
            {
                continue;
            }

            let mut disk = NbtCompound::new();
            disk.put_string("name", name.into());
            disk.put_compound(
                "states",
                entry
                    .get_compound("states")
                    .ok_or_else(|| invalid("missing block states"))?
                    .clone(),
            );
            disk.put_int(
                "version",
                entry
                    .get_int("version")
                    .ok_or_else(|| invalid("missing block version"))?,
            );
            let mut bytes = Vec::new();
            Nbt::new(String::new(), disk).write_to_writer_bedrock_disk(&mut bytes)?;
            states.insert(state, DiskState { preference, bytes });
        }
        Ok(Self(states))
    }

    fn get(&self, state: BlockStateId) -> Result<&[u8], Error> {
        self.0
            .get(&state)
            .map(|entry| entry.bytes.as_slice())
            .ok_or_else(|| {
                invalid(format!(
                    "block {} has no reversible Bedrock state",
                    state.to_block().name
                ))
            })
    }
}

struct NativeLayer<'a> {
    indices: Vec<usize>,
    palette: Vec<&'a [u8]>,
}

impl<'a> NativeLayer<'a> {
    fn uniform(entry: &'a [u8]) -> Self {
        Self {
            indices: vec![0; BlockPalette::VOLUME],
            palette: vec![entry],
        }
    }

    fn read(reader: &mut DiskReader<'a>) -> Result<Self, Error> {
        let header = reader.get_u8()?;
        if header & 1 != 0 {
            return Err(invalid("network block palette in disk subchunk"));
        }
        let bits = header >> 1;
        let mut indices = read_indices(reader, bits)?;
        if indices.is_empty() {
            indices.resize(BlockPalette::VOLUME, 0);
        }
        let count = read_palette_len(reader, bits)?;
        let mut palette = Vec::with_capacity(count);
        let bytes = *reader.reader().get_ref();
        for _ in 0..count {
            let start = reader.reader().position() as usize;
            Nbt::read(reader)?;
            let end = reader.reader().position() as usize;
            palette.push(&bytes[start..end]);
        }
        if indices.iter().any(|&value| value >= palette.len()) {
            return Err(invalid("disk palette index out of range"));
        }
        Ok(Self { indices, palette })
    }

    fn replace(&mut self, index: usize, state: &'a [u8]) -> Result<(), Error> {
        let cell = self
            .indices
            .get_mut(index)
            .ok_or_else(|| invalid("invalid edited block index"))?;
        *cell = if let Some(value) = self.palette.iter().position(|&existing| existing == state) {
            value
        } else {
            self.palette.push(state);
            self.palette.len() - 1
        };
        Ok(())
    }

    fn write(&self, out: &mut Vec<u8>) {
        // Unused entries can remain after a move. Compact them in cell order
        // and keep each surviving original NBT entry byte for byte.
        let mut active = Vec::<&[u8]>::new();
        let mut remap = vec![None; self.palette.len()];
        let indices: Vec<u32> = self
            .indices
            .iter()
            .map(|&old| {
                *remap[old].get_or_insert_with(|| {
                    let index = active.len() as u32;
                    active.push(self.palette[old]);
                    index
                })
            })
            .collect();

        let bits = bedrock_palette_bits(active.len());
        out.push(bits << 1);
        if bits != 0 {
            for values in indices.chunks(32 / usize::from(bits)) {
                let word = values.iter().enumerate().fold(0, |word, (index, value)| {
                    word | value << (usize::from(bits) * index)
                });
                out.extend_from_slice(&word.to_le_bytes());
            }
            out.extend_from_slice(&(active.len() as u32).to_le_bytes());
        }
        for entry in active {
            out.extend_from_slice(entry);
        }
    }
}

/// Rewrites selected block cells while retaining the native entries of other cells.
pub(in crate::chunk::io::bedrock_import) fn rewrite_subchunk<'a>(
    raw: Option<&'a [u8]>,
    y: i8,
    changes: &[(usize, BlockStateId)],
    states: &'a DiskBlockStates,
) -> Result<Vec<u8>, Error> {
    let air = states.get(BlockStateId::AIR)?;
    let mut primary = NativeLayer::uniform(air);
    let mut secondary = None;
    if let Some(raw) = raw {
        let mut reader = NbtReadHelperBedrockDisk::new(Cursor::new(raw));
        let version = reader.get_u8()?;
        let layers = reader.get_u8()?;
        if version != SUBCHUNK_VERSION || layers > 2 || reader.get_i8()? != y {
            return Err(invalid("invalid disk subchunk header"));
        }
        if layers > 0 {
            primary = NativeLayer::read(&mut reader)?;
        }
        if layers == 2 {
            secondary = Some(NativeLayer::read(&mut reader)?);
        }
        if reader.reader().position() != raw.len() as u64 {
            return Err(invalid("trailing data in subchunk"));
        }
    }
    for &(index, value) in changes {
        primary.replace(index, states.get(value)?)?;
        if let Some(layer) = &mut secondary {
            layer.replace(index, air)?;
        }
    }
    let mut written = vec![
        SUBCHUNK_VERSION,
        1 + u8::from(secondary.is_some()),
        y.cast_unsigned(),
    ];
    primary.write(&mut written);
    if let Some(layer) = secondary {
        layer.write(&mut written);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_data::Block;

    #[test]
    fn imports_unspecified_standing_torches_and_writes_the_top_direction() {
        let registry = TerrainRegistry::load().unwrap();
        let disk = DiskBlockStates::load(&registry).unwrap();
        for (name, block) in [
            ("minecraft:torch", Block::TORCH),
            ("minecraft:soul_torch", Block::SOUL_TORCH),
            ("minecraft:copper_torch", Block::COPPER_TORCH),
        ] {
            let mut properties = NbtCompound::new();
            properties.put_string("torch_facing_direction", "unknown".into());
            let mut old = NbtCompound::new();
            old.put_string("name", name.into());
            old.put_compound("states", properties);
            let state = block.default_state.id;
            assert_eq!(registry.block(&old).unwrap(), state);

            let mut reader = DiskReader::new(Cursor::new(disk.get(state).unwrap()));
            let written = Nbt::read(&mut reader).unwrap();
            assert_eq!(written.get_string("name"), Some(name));
            assert_eq!(
                written
                    .get_compound("states")
                    .unwrap()
                    .get_string("torch_facing_direction"),
                Some("top"),
            );
            assert_eq!(registry.block(&written).unwrap(), state);
        }
    }
}
