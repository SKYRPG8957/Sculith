use std::{collections::HashMap, io::Cursor};

use pumpkin_data::{Block, BlockState, BlockStateId, Property, biome::Biome};
use pumpkin_nbt::{
    Nbt, NbtCompound,
    deserializer::{NbtReadHelper, NbtReadHelperBedrock, NbtReadHelperBedrockDisk},
    tag::NbtTag,
};

use crate::chunk::palette::{BiomePalette, BlockPalette, bedrock_water_state};

use super::{Error, invalid};

const BLOCK_STATES: &[u8] = include_bytes!("../../../../../../assets/bedrock/block_states.nbt");
const BIOME_IDS: &str = include_str!("../../../../../../assets/bedrock/biomes.json");
const SUBCHUNK_VERSION: u8 = 9;
const BIOME_INHERIT: u8 = 0x7f;
const HEIGHTMAP_BYTES: usize = 16 * 16 * 2;

#[derive(Hash, PartialEq, Eq)]
struct BlockKey {
    name: String,
    properties: Vec<(Box<str>, BlockProperty)>,
}

#[derive(Hash, PartialEq, Eq)]
enum BlockProperty {
    Byte(i8),
    Int(i32),
    String(Box<str>),
}

impl BlockKey {
    fn read(compound: &NbtCompound) -> Result<Self, Error> {
        let name = compound
            .get_string("name")
            .ok_or_else(|| invalid("block palette entry has no name"))?;
        let states = compound
            .get_compound("states")
            .ok_or_else(|| invalid("block palette entry has no states"))?;
        let mut properties = Vec::with_capacity(states.child_tags.len());
        for (key, value) in &states.child_tags {
            let property = match value {
                NbtTag::Byte(value) => BlockProperty::Byte(*value),
                NbtTag::Int(value) => BlockProperty::Int(*value),
                NbtTag::String(value) => BlockProperty::String(value.clone()),
                _ => return Err(invalid(format!("unsupported block property {name}.{key}"))),
            };
            properties.push((key.clone(), property));
        }
        properties.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        Ok(Self {
            name: name.to_owned(),
            properties,
        })
    }

    fn liquid_state(&self) -> Option<BlockStateId> {
        let block = match self.name.as_str() {
            "minecraft:water" | "minecraft:flowing_water" => &Block::WATER,
            "minecraft:lava" | "minecraft:flowing_lava" => &Block::LAVA,
            _ => return None,
        };
        let [(property, BlockProperty::Int(depth))] = self.properties.as_slice() else {
            return None;
        };
        if property.as_ref() != "liquid_depth" {
            return None;
        }
        let depth = u8::try_from(*depth).ok()?;
        // Both Bedrock identifiers describe the same set of shared fluid levels.
        block
            .state_from_properties(&[Property::Level(depth)])
            .map(|state| state.id)
    }
}

pub(super) struct TerrainRegistry {
    blocks: HashMap<BlockKey, BlockStateId>,
    biomes: HashMap<u32, u8>,
}

impl TerrainRegistry {
    pub(super) fn load() -> Result<Self, Error> {
        #[derive(serde::Deserialize)]
        struct BiomeMapping {
            bedrock_id: u32,
        }

        let mut candidates = HashMap::<u32, Vec<BlockStateId>>::new();
        for state in (0..BlockStateId::COUNT).filter_map(BlockStateId::new) {
            candidates
                .entry(BlockState::to_be_network_id(state))
                .or_default()
                .push(state);
        }
        let mut blocks = HashMap::new();
        let mut reader = NbtReadHelperBedrock::new(Cursor::new(BLOCK_STATES));
        while reader.reader().position() < BLOCK_STATES.len() as u64 {
            let entry = Nbt::read(&mut reader)?;
            let key = BlockKey::read(&entry)?;
            let network_id = entry
                .get_int("network_id")
                .ok_or_else(|| invalid("block state registry entry has no network ID"))?
                .cast_unsigned();
            if key.name == "minecraft:air" {
                blocks.insert(key, BlockStateId::AIR);
                continue;
            }
            let name = key.name.strip_prefix("minecraft:").unwrap_or(&key.name);
            if let Some(state) = candidates.get(&network_id).and_then(|states| {
                // Prefer the same block name, the dry state and its default properties
                // when several Java states share a Bedrock identifier.
                states.iter().rev().copied().max_by_key(|id| {
                    let block = Block::from_state_id(*id);
                    (
                        block.name == name,
                        !id.is_waterlogged(),
                        *id == block.default_state.id,
                    )
                })
            }) {
                blocks.insert(key, state);
            }
        }

        let biome_mappings =
            serde_json::from_str::<std::collections::BTreeMap<String, BiomeMapping>>(BIOME_IDS)?;
        let mut biomes = HashMap::new();
        for (name, mapping) in biome_mappings {
            if let Some(biome) = Biome::from_name(name.strip_prefix("minecraft:").unwrap_or(&name))
            {
                biomes.entry(mapping.bedrock_id).or_insert(biome.id);
            }
        }
        Ok(Self { blocks, biomes })
    }

    pub(super) fn biome(&self, id: u32) -> Result<u8, Error> {
        self.biomes
            .get(&id)
            .copied()
            .ok_or_else(|| invalid(format!("Bedrock biome {id} has no mapping")))
    }

    fn block(&self, entry: &NbtCompound) -> Result<BlockStateId, Error> {
        let key = BlockKey::read(entry)?;
        self.blocks
            .get(&key)
            .copied()
            .or_else(|| key.liquid_state())
            .ok_or_else(|| invalid(format!("Bedrock block state {} has no mapping", key.name)))
    }
}

pub(super) fn subchunk(
    bytes: &[u8],
    y: i8,
    registry: &TerrainRegistry,
) -> Result<BlockPalette, Error> {
    let mut reader = NbtReadHelperBedrockDisk::new(Cursor::new(bytes));
    let version = reader.get_u8()?;
    if version != SUBCHUNK_VERSION {
        return Err(invalid(format!("unsupported subchunk version {version}")));
    }
    let layers = reader.get_u8()?;
    if !matches!(layers, 1 | 2) || reader.get_i8()? != y {
        return Err(invalid("invalid subchunk layer count or vertical index"));
    }

    let mut primary = read_block_layer(&mut reader, registry)?;
    if layers == 2 {
        let secondary = read_block_layer(&mut reader, registry)?;
        for (state, fluid) in primary.iter_mut().zip(secondary) {
            *state = merge_layers(*state, fluid)?;
        }
    }
    if reader.reader().position() != bytes.len() as u64 {
        return Err(invalid("trailing data in subchunk"));
    }
    Ok(BlockPalette::from_fn(|x, y, z| {
        primary[bedrock_index(x, y, z)]
    }))
}

fn read_block_layer(
    reader: &mut NbtReadHelperBedrockDisk<Cursor<&[u8]>>,
    registry: &TerrainRegistry,
) -> Result<Vec<BlockStateId>, Error> {
    let header = reader.get_u8()?;
    if header & 1 != 0 {
        return Err(invalid("network block palette in disk subchunk"));
    }
    let bits = header >> 1;
    let indices = read_indices(reader, bits)?;
    let len = read_palette_len(reader, bits)?;
    let mut palette = Vec::with_capacity(len);
    for _ in 0..len {
        palette.push(registry.block(&Nbt::read(reader)?.root_tag)?);
    }
    apply_palette(indices, &palette)
}

fn merge_layers(primary: BlockStateId, secondary: BlockStateId) -> Result<BlockStateId, Error> {
    if secondary == BlockStateId::AIR {
        return Ok(primary);
    }
    if primary == BlockStateId::AIR {
        return Ok(secondary);
    }
    if secondary == Block::WATER.default_state.id {
        if bedrock_water_state(primary) == secondary {
            return Ok(primary);
        }
        if let Some(wet) = Block::from_state_id(primary).set_waterlogged(primary, true) {
            return Ok(wet);
        }
    }
    Err(invalid("secondary block layer has no shared block state"))
}

pub(super) fn biomes(
    bytes: &[u8],
    section_count: usize,
    registry: &TerrainRegistry,
) -> Result<Box<[BiomePalette]>, Error> {
    let data = bytes
        .get(HEIGHTMAP_BYTES..)
        .ok_or_else(|| invalid("truncated biome heightmap"))?;
    let mut reader = NbtReadHelperBedrockDisk::new(Cursor::new(data));
    let mut sections = Vec::<BiomePalette>::with_capacity(section_count);
    for _ in 0..section_count {
        // Saved biomes use fixed-width registry IDs even when the low bit is set.
        let bits = reader.get_u8()? >> 1;
        let section = if bits == BIOME_INHERIT {
            sections
                .last()
                .cloned()
                .ok_or_else(|| invalid("first biome section cannot inherit"))?
        } else {
            let indices = read_indices(&mut reader, bits)?;
            let len = read_palette_len(&mut reader, bits)?;
            let mut palette = Vec::with_capacity(len);
            for _ in 0..len {
                palette.push(registry.biome(reader.get_i32()?.cast_unsigned())?);
            }
            // Anvil biome cells cover four blocks per axis. Sample the matching
            // cell origin in the Bedrock storage when making the shared palette.
            if palette.len() == 1 && indices.iter().all(|&index| index == 0) {
                BiomePalette::Homogeneous(palette[0])
            } else {
                let values = apply_palette(indices, &palette)?;
                BiomePalette::from_fn(|x, y, z| values[bedrock_index(x * 4, y * 4, z * 4)])
            }
        };
        sections.push(section);
    }
    if reader.reader().position() != data.len() as u64 {
        return Err(invalid("trailing data in biome sections"));
    }
    Ok(sections.into_boxed_slice())
}

fn read_indices(
    reader: &mut NbtReadHelperBedrockDisk<Cursor<&[u8]>>,
    bits: u8,
) -> Result<Vec<usize>, Error> {
    if !matches!(bits, 0..=6 | 8 | 16) {
        return Err(invalid(format!("invalid disk palette width {bits}")));
    }
    if bits == 0 {
        return Ok(Vec::new());
    }
    let mut indices = Vec::with_capacity(BlockPalette::VOLUME);
    let per_word = 32 / u32::from(bits);
    let mask = (1 << bits) - 1;
    while indices.len() < BlockPalette::VOLUME {
        let word = reader.get_i32()?.cast_unsigned();
        let count = per_word.min((BlockPalette::VOLUME - indices.len()) as u32);
        indices.extend((0..count).map(|i| ((word >> (i * u32::from(bits))) & mask) as usize));
    }
    Ok(indices)
}

fn read_palette_len(
    reader: &mut NbtReadHelperBedrockDisk<Cursor<&[u8]>>,
    bits: u8,
) -> Result<usize, Error> {
    if bits == 0 {
        return Ok(1);
    }
    let len = reader.get_i32()?.cast_unsigned() as usize;
    if len == 0 || len > BlockPalette::VOLUME.min(1 << bits) {
        return Err(invalid(format!("invalid disk palette length {len}")));
    }
    Ok(len)
}

fn apply_palette<T: Copy>(indices: Vec<usize>, palette: &[T]) -> Result<Vec<T>, Error> {
    if indices.is_empty() {
        return Ok(vec![palette[0]; BlockPalette::VOLUME]);
    }
    indices
        .into_iter()
        .map(|index| {
            palette
                .get(index)
                .copied()
                .ok_or_else(|| invalid("disk palette index out of range"))
        })
        .collect()
}

const fn bedrock_index(x: usize, y: usize, z: usize) -> usize {
    (x * 16 + z) * 16 + y
}
