//! V2225 overworld chunk encoding for the current five-block internal registry.
//!
//! Protocol facts checked against MIT-licensed Dragonfly commit
//! 0f9a3a5736267db60bf46e92d408f81be1de9a64:
//! - server/world/network_block_hash.go (canonical LE NBT and FNV-1a32)
//! - server/world/chunk/{encode,encoding}.go (v9 subchunks and network palettes)
//!
//! https://github.com/df-mc/dragonfly/tree/0f9a3a5736267db60bf46e92d408f81be1de9a64/server/world
//!
//! This is an independent implementation of those protocol facts, not copied
//! Dragonfly implementation code. Packet fields are supplied by Apache-2.0
//! bedrock-rs commit 5221fcae8cc8468f523f3f58f129cb308e9d3369.

use anyhow::{Context, Result, ensure};
use bedrock::protocol::{V2225, v662::types::ChunkPos, v2225::packets::LevelChunkPacket};
use sculith_world::{BlockKind, ChunkSnapshot, SECTION_BLOCKS, SECTION_COUNT, Section};

const MAX_ENCODED_BYTES: usize = 65_536;
const SUBCHUNK_VERSION: u8 = 9;

/// Encodes all 24 subchunks, plains biomes and an empty border-block list.
/// `StartGame.block_network_ids_are_hashes` must be true for this palette.
/// The snapshot's internal packed storage is never sent as Minecraft wire data.
pub fn encode_chunk(snapshot: &ChunkSnapshot) -> Result<LevelChunkPacket<V2225>> {
    let mut serialized = Vec::with_capacity(2048);
    let mut indices = Vec::with_capacity(SECTION_BLOCKS);
    for (section_y, section) in snapshot.sections() {
        let section_y =
            i8::try_from(section_y).context("section Y cannot fit the v9 signed byte")?;
        serialized.extend_from_slice(&[SUBCHUNK_VERSION, 1, section_y as u8]);
        encode_storage(section, &mut indices, &mut serialized)?;
        ensure!(
            serialized.len() <= MAX_ENCODED_BYTES,
            "encoded chunk exceeds 64 KiB"
        );
    }
    // One uniform plains biome storage, followed by the network "reuse previous"
    // marker for the other vertical sections. Plains ID 1 uses signed VarInt 2.
    serialized.extend_from_slice(&[1, 2]);
    serialized.extend(std::iter::repeat_n(0xff, SECTION_COUNT - 1));
    serialized.push(0); // no border blocks; no block entities follow
    ensure!(
        serialized.len() <= MAX_ENCODED_BYTES,
        "encoded chunk exceeds 64 KiB"
    );
    let position = snapshot.position();
    Ok(LevelChunkPacket {
        chunk_position: ChunkPos {
            x: position.x,
            z: position.z,
        },
        dimension_id: 0,
        sub_chunk_count: SECTION_COUNT as u32,
        client_request_sub_chunk_limit: None,
        cache_enabled: false,
        cache_blobs: vec![],
        serialized_chunk_data: serialized,
        is_client_biome_update: false,
    })
}

fn encode_storage(section: &Section, indices: &mut Vec<u8>, output: &mut Vec<u8>) -> Result<()> {
    if section.stats().uniform {
        output.push(1); // 0 bits per block; network flag set
        write_signed_varint(block_hash(section.get(0, 0, 0)?)? as i32, output);
        return Ok(());
    }

    indices.clear();
    let mut palette = Vec::with_capacity(BlockKind::ALL.len());
    let mut reverse = [u8::MAX; BlockKind::ALL.len()];
    // Bedrock's linear order is Y-fast, then Z, then X: x*256 + z*16 + y.
    // sculith-world uses a different order, so explicit coordinate traversal is required.
    for x in 0..16 {
        for z in 0..16 {
            for y in 0..16 {
                let kind = section.get(x, y, z)?;
                let index = &mut reverse[kind as usize];
                if *index == u8::MAX {
                    *index = palette.len() as u8;
                    palette.push(kind);
                }
                indices.push(*index);
            }
        }
    }
    ensure!(
        palette.len() <= 5,
        "block registry exceeds this encoder's palette bound"
    );
    let bits = match palette.len() {
        1 => 0,
        2 => 1,
        3 | 4 => 2,
        5 => 3,
        _ => anyhow::bail!("invalid section palette"),
    };
    output.push((bits << 1) | 1);
    if bits != 0 {
        // Values never cross a u32 boundary: a three-bit palette uses 10 values
        // per word, leaving its top two bits unused rather than carrying across.
        let per_word = 32 / usize::from(bits);
        for entries in indices.chunks(per_word) {
            let mut word = 0u32;
            for (slot, &value) in entries.iter().enumerate() {
                word |= u32::from(value) << (slot * usize::from(bits));
            }
            output.extend_from_slice(&word.to_le_bytes());
        }
        write_signed_varint(palette.len() as i32, output);
    }
    for kind in palette {
        write_signed_varint(block_hash(kind)? as i32, output);
    }
    Ok(())
}

fn write_signed_varint(value: i32, output: &mut Vec<u8>) {
    let mut encoded = ((value as u32) << 1) ^ ((value >> 31) as u32);
    while encoded >= 0x80 {
        output.push((encoded as u8 & 0x7f) | 0x80);
        encoded >>= 7;
    }
    output.push(encoded as u8);
}

pub(super) fn block_hash(kind: BlockKind) -> Result<u32> {
    match kind {
        BlockKind::Air => network_hash("minecraft:air", &[]),
        BlockKind::Bedrock => network_hash("minecraft:bedrock", &[("infiniburn_bit", 0)]),
        BlockKind::Stone => network_hash("minecraft:stone", &[]),
        BlockKind::Dirt => network_hash("minecraft:dirt", &[]),
        BlockKind::Grass => network_hash("minecraft:grass_block", &[]),
    }
}

/// Current registry states contain only byte properties. More state NBT types
/// must be explicitly implemented when the internal block registry expands.
fn network_hash(name: &str, byte_states: &[(&str, i8)]) -> Result<u32> {
    ensure!(
        byte_states.len() <= 64,
        "block state property count exceeds 64"
    );
    let mut states = byte_states.to_vec();
    states.sort_unstable_by_key(|&(name, _)| name);
    ensure!(
        !states.windows(2).any(|pair| pair[0].0 == pair[1].0),
        "duplicate block state property"
    );

    let mut hash = Fnv1a32(2_166_136_261);
    hash.bytes(&[10, 0, 0]); // unnamed compound root
    hash.bytes(&[8]);
    hash.string("name")?;
    hash.string(name)?;
    hash.bytes(&[10]);
    hash.string("states")?;
    for (key, value) in states {
        hash.bytes(&[1]);
        hash.string(key)?;
        hash.bytes(&[value as u8]);
    }
    hash.bytes(&[0, 0]); // end states and root; deliberately no version field
    Ok(hash.0)
}

struct Fnv1a32(u32);

impl Fnv1a32 {
    fn bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 ^ u32::from(byte)).wrapping_mul(16_777_619);
        }
    }

    fn string(&mut self, value: &str) -> Result<()> {
        let length = u16::try_from(value.len()).context("NBT name/string exceeds 65535 bytes")?;
        self.bytes(&length.to_le_bytes());
        self.bytes(value.as_bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bedrock::protocol::{PacketHeader, Packets};
    use sculith_world::{BlockEdit, BlockPos, ChunkPos as WorldChunkPos, FlatGenerator, Region};
    use std::io::{Cursor, Read};

    // Factual golden hashes independently derived from Dragonfly's canonical NBT
    // block_states.nbt at the exact commit in the module source comment.
    fn expected_hash(kind: BlockKind) -> u32 {
        match kind {
            BlockKind::Air => 3_690_217_760,
            BlockKind::Bedrock => 4_121_722_107,
            BlockKind::Stone => 2_150_698_529,
            BlockKind::Dirt => 2_186_211_206,
            BlockKind::Grass => 3_727_763_636,
        }
    }

    struct DecodedSection {
        y: i8,
        hashes: Vec<u32>,
        bits: u8,
    }

    fn read_byte(input: &mut Cursor<&[u8]>) -> Result<u8> {
        let mut byte = [0];
        input.read_exact(&mut byte)?;
        Ok(byte[0])
    }

    // Test-only decoder intentionally does not reuse the encoder's VarInt,
    // storage, indexing or hashing helpers.
    fn read_signed(input: &mut Cursor<&[u8]>) -> Result<i32> {
        let mut value = 0u32;
        for shift in [0, 7, 14, 21, 28] {
            let byte = read_byte(input)?;
            ensure!(shift != 28 || byte & 0xf0 == 0, "VarInt overflow");
            value |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(((value >> 1) as i32) ^ -((value & 1) as i32));
            }
        }
        anyhow::bail!("VarInt too long")
    }

    fn decode_storage(input: &mut Cursor<&[u8]>) -> Result<(u8, Vec<u32>)> {
        let header = read_byte(input)?;
        ensure!(
            header & 1 == 1 && header != 0xff,
            "expected network storage"
        );
        let bits = header >> 1;
        if bits == 0 {
            return Ok((0, vec![read_signed(input)? as u32; 4096]));
        }
        ensure!(matches!(bits, 1..=3), "unexpected bits per block");
        let capacity = 32 / usize::from(bits);
        let mut words = Vec::new();
        for _ in 0..4096usize.div_ceil(capacity) {
            let mut bytes = [0; 4];
            input.read_exact(&mut bytes)?;
            words.push(u32::from_le_bytes(bytes));
        }
        let count = read_signed(input)?;
        ensure!((1..=5).contains(&count), "invalid palette count");
        let mut palette = Vec::new();
        for _ in 0..count {
            palette.push(read_signed(input)? as u32);
        }
        let mut hashes = Vec::with_capacity(4096);
        for index in 0..4096 {
            let word = words[index / capacity];
            let slot = index % capacity;
            let palette_index = ((word >> (slot * usize::from(bits))) & ((1 << bits) - 1)) as usize;
            hashes.push(
                *palette
                    .get(palette_index)
                    .context("palette index out of range")?,
            );
        }
        Ok((bits, hashes))
    }

    fn decode_payload(bytes: &[u8]) -> Result<Vec<DecodedSection>> {
        let mut input = Cursor::new(bytes);
        let mut sections = Vec::new();
        for _ in 0..24 {
            ensure!(read_byte(&mut input)? == 9, "subchunk version");
            ensure!(read_byte(&mut input)? == 1, "subchunk storage count");
            let y = read_byte(&mut input)? as i8;
            let (bits, hashes) = decode_storage(&mut input)?;
            sections.push(DecodedSection { y, hashes, bits });
        }
        let (bits, biome) = decode_storage(&mut input)?;
        ensure!(
            bits == 0 && biome.iter().all(|&id| id == 1),
            "expected uniform plains biome"
        );
        for _ in 1..24 {
            ensure!(
                read_byte(&mut input)? == 0xff,
                "expected reuse-previous biome"
            );
        }
        ensure!(read_byte(&mut input)? == 0, "expected zero border blocks");
        ensure!(
            input.position() as usize == bytes.len(),
            "unexpected trailing chunk bytes"
        );
        Ok(sections)
    }

    fn compare_all_voxels(snapshot: &ChunkSnapshot, decoded: &[DecodedSection]) {
        assert_eq!(decoded.len(), 24);
        for (slot, section) in decoded.iter().enumerate() {
            assert_eq!(i32::from(section.y), slot as i32 - 4);
            for x in 0..16u8 {
                for y in 0..16u8 {
                    for z in 0..16u8 {
                        let world_y = i32::from(section.y) * 16 + i32::from(y);
                        let expected = expected_hash(snapshot.get(x, world_y, z).unwrap());
                        let wire_index =
                            usize::from(x) * 256 + usize::from(z) * 16 + usize::from(y);
                        assert_eq!(
                            section.hashes[wire_index], expected,
                            "voxel ({x},{world_y},{z})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn canonical_nbt_hashes_match_verified_vanilla_states() {
        for kind in [
            BlockKind::Air,
            BlockKind::Bedrock,
            BlockKind::Stone,
            BlockKind::Dirt,
            BlockKind::Grass,
        ] {
            assert_eq!(block_hash(kind).unwrap(), expected_hash(kind), "{kind:?}");
        }
        assert_eq!(
            network_hash("sculith:test", &[("z", 1), ("a", -1)]).unwrap(),
            network_hash("sculith:test", &[("a", -1), ("z", 1)]).unwrap()
        );
        assert!(network_hash("sculith:test", &[("same", 0), ("same", 1)]).is_err());
        assert!(network_hash(&"x".repeat(65_536), &[]).is_err());
    }

    #[test]
    fn all_flat_voxels_and_negative_sections_decode_independently() {
        let snapshot = FlatGenerator::default()
            .snapshot(WorldChunkPos { x: -2, z: -3 })
            .unwrap();
        let packet = encode_chunk(&snapshot).unwrap();
        assert_eq!((packet.chunk_position.x, packet.chunk_position.z), (-2, -3));
        assert_eq!(packet.sub_chunk_count, 24);
        assert_eq!(packet.dimension_id, 0);
        assert!(packet.client_request_sub_chunk_limit.is_none());
        assert!(!packet.cache_enabled && !packet.is_client_biome_update);
        assert!(packet.cache_blobs.is_empty());
        let decoded = decode_payload(&packet.serialized_chunk_data).unwrap();
        compare_all_voxels(&snapshot, &decoded);
        assert_eq!(decoded[4].bits, 2);
        assert!(
            decoded
                .iter()
                .enumerate()
                .all(|(i, section)| i == 4 || section.bits == 0)
        );
        assert!(packet.serialized_chunk_data.len() < MAX_ENCODED_BYTES);
    }

    #[test]
    fn asymmetric_edits_preserve_axes_and_three_bit_u32_padding() {
        let mut owner = Region::flat_origin();
        let position = WorldChunkPos { x: -2, z: -3 };
        let before = owner.snapshot(position).unwrap();
        owner
            .commit(
                0,
                &[
                    BlockEdit {
                        position: BlockPos {
                            x: -17,
                            y: -63,
                            z: -34,
                        },
                        block: BlockKind::Stone,
                    },
                    BlockEdit {
                        position: BlockPos {
                            x: -30,
                            y: 47,
                            z: -47,
                        },
                        block: BlockKind::Dirt,
                    },
                    BlockEdit {
                        position: BlockPos {
                            x: -32,
                            y: 5,
                            z: -47,
                        },
                        block: BlockKind::Stone,
                    },
                ],
            )
            .unwrap();
        let snapshot = owner.snapshot(position).unwrap();
        let encoded = encode_chunk(&snapshot).unwrap();
        let decoded = decode_payload(&encoded.serialized_chunk_data).unwrap();
        compare_all_voxels(&snapshot, &decoded);
        assert_eq!(decoded[0].bits, 1);
        assert_eq!(decoded[4].bits, 3);
        assert_eq!(decoded[6].bits, 1);
        assert_eq!(decoded[4].hashes[21], expected_hash(BlockKind::Stone));
        assert_eq!(before.get(0, 5, 1).unwrap(), BlockKind::Air);
    }

    #[test]
    fn newest_level_chunk_packet_codec_roundtrip_preserves_payload_and_coordinates() {
        let snapshot = FlatGenerator::default()
            .snapshot(WorldChunkPos { x: -7, z: 9 })
            .unwrap();
        let packet = encode_chunk(&snapshot).unwrap();
        let expected_payload = packet.serialized_chunk_data.clone();
        let wrapper = V2225::LevelChunkPacket(Box::new(packet));
        let header = PacketHeader {
            packet_id: 58,
            ..PacketHeader::default()
        };
        let mut bytes = Vec::new();
        wrapper.serialize(&header, &mut bytes).unwrap();
        let mut cursor = Cursor::new(bytes.as_slice());
        let (decoded, decoded_header) = V2225::deserialize(&mut cursor).unwrap();
        assert_eq!(decoded_header.packet_id, 58);
        assert_eq!(cursor.position() as usize, bytes.len());
        let V2225::LevelChunkPacket(packet) = decoded else {
            panic!("wrong decoded packet type")
        };
        assert_eq!((packet.chunk_position.x, packet.chunk_position.z), (-7, 9));
        assert_eq!((packet.dimension_id, packet.sub_chunk_count), (0, 24));
        assert!(packet.client_request_sub_chunk_limit.is_none());
        assert!(!packet.cache_enabled && !packet.is_client_biome_update);
        assert!(packet.cache_blobs.is_empty());
        assert_eq!(packet.serialized_chunk_data, expected_payload);
        compare_all_voxels(
            &snapshot,
            &decode_payload(&packet.serialized_chunk_data).unwrap(),
        );
    }
}
