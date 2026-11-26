use bytes::Bytes;
use std::{
    collections::VecDeque,
    hash::BuildHasher,
    io::{Error, Write},
    sync::{Arc, Mutex, OnceLock, Weak},
};
use xxhash_rust::xxh64::xxh64;

use pumpkin_data::dimension::Dimension;
use pumpkin_nbt::{Nbt, compound::NbtCompound};
use sculith_world::chunk::{
    BedrockSectionBlobs, ChunkData,
    palette::{BeNetworkSerialization, NetworkPalette},
};

use sculith_protocol::{
    codec::{var_int::VarInt, var_uint::VarUInt},
    serial::PacketWrite,
};

use crate::entity::player::bedrock_dimension_id;

const VERSION: u8 = 9;
static NETHER_DIMENSION: Dimension = Dimension::THE_NETHER;
const NETHER_SECTIONS: usize = (NETHER_DIMENSION.logical_height / 16) as usize;

fn write_block_storage(
    writer: &mut Vec<u8>,
    network_repr: BeNetworkSerialization<u32>,
) -> Result<(), Error> {
    (network_repr.bits_per_entry << 1 | 1).write(writer)?;

    for data in network_repr.packed_data {
        data.write(writer)?;
    }

    // Palette entries are block network IDs, which Bedrock serializes as signed varints.
    match network_repr.palette {
        NetworkPalette::Single(id) => VarInt(id as i32).write(writer)?,
        NetworkPalette::Indirect(palette) => {
            VarInt(palette.len() as i32).write(writer)?;
            for id in palette {
                VarInt(id as i32).write(writer)?;
            }
        }
        NetworkPalette::Direct => {}
    }
    Ok(())
}

pub struct CLevelChunk<'a> {
    // https://mojang.github.io/bedrock-protocol-docs/html/LevelChunkPacket.html
    pub dimension: i32,
    pub cache_enabled: bool,

    // https://gist.github.com/Tomcc/a96af509e275b1af483b25c543cfbf37
    // https://github.com/Mojang/bedrock-protocol-docs/blob/main/additional_docs/SubChunk%20Request%20System%20v1.18.10.md
    pub chunk: &'a ChunkData,
    pub block_actors: &'a [NbtCompound],
}

impl sculith_protocol::packet::Packet for CLevelChunk<'_> {
    const PACKET_ID: i32 = pumpkin_data::bedrock_packet::LEVEL_CHUNK;
}

pub type ChunkBlob = (u64, Bytes);
pub type EncodedChunk = (Vec<u8>, Vec<ChunkBlob>);

/// A world retains at most 8 MiB of inline terrain. Entries never pin loaded chunks.
#[derive(Default)]
pub(crate) struct InlineChunkCache {
    shards: OnceLock<Box<[Mutex<VecDeque<InlineFrame>>]>>,
}

struct InlineFrame {
    chunk: Weak<ChunkData>,
    dimension: i32,
    revision: (u64, u64),
    bytes: Bytes,
}

impl InlineChunkCache {
    const SHARDS: usize = 128;
    const ENTRIES_PER_SHARD: usize = 4;
    const BYTES_PER_SHARD: usize = 64 * 1024;

    fn index(chunk: &ChunkData) -> usize {
        rustc_hash::FxBuildHasher.hash_one((chunk.x, chunk.z)) as usize % Self::SHARDS
    }

    pub(super) fn get(&self, chunk: &Arc<ChunkData>, dimension: i32) -> Option<Bytes> {
        let shard = self.shards.get()?[Self::index(chunk)]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let revision = chunk.section.terrain_revision();
        shard
            .iter()
            .find(|frame| {
                frame.chunk.as_ptr() == Arc::as_ptr(chunk)
                    && frame.dimension == dimension
                    && frame.revision == revision
            })
            .map(|frame| frame.bytes.clone())
    }

    pub(super) fn insert(
        &self,
        chunk: &Arc<ChunkData>,
        dimension: i32,
        revision: (u64, u64),
        bytes: &Bytes,
    ) {
        if bytes.len() > Self::BYTES_PER_SHARD || chunk.section.terrain_revision() != revision {
            return;
        }
        let shards = self
            .shards
            .get_or_init(|| (0..Self::SHARDS).map(|_| Mutex::default()).collect());
        let mut shard = shards[Self::index(chunk)]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shard.retain(|frame| frame.chunk.as_ptr() != Arc::as_ptr(chunk));
        let mut size = bytes.len() + shard.iter().map(|frame| frame.bytes.len()).sum::<usize>();
        while size > Self::BYTES_PER_SHARD || shard.len() >= Self::ENTRIES_PER_SHARD {
            let Some(oldest) = shard.pop_front() else {
                break;
            };
            size -= oldest.bytes.len();
        }
        shard.push_back(InlineFrame {
            chunk: Arc::downgrade(chunk),
            dimension,
            revision,
            bytes: bytes.clone(),
        });
    }
}

fn blob(data: Vec<u8>) -> ChunkBlob {
    (xxh64(&data, 0), Bytes::from(data.into_boxed_slice()))
}

fn encode_block_actors<'a>(
    block_actors: impl IntoIterator<Item = &'a NbtCompound>,
) -> Result<Vec<u8>, Error> {
    let mut encoded = Vec::new();
    for block_actor in block_actors {
        encoded.write_all(&Nbt::from(block_actor.clone()).write_bedrock())?;
    }
    Ok(encoded)
}

fn encode_biomes(chunk: &ChunkData) -> Result<BedrockSectionBlobs, Error> {
    chunk.section.biome_sections.bedrock_blobs(|biomes| {
        let mut data = Vec::new();
        let mut ceiling = None;
        for (index, palette) in biomes.iter().enumerate() {
            let network_repr = palette.convert_be_network();
            (network_repr.bits_per_entry << 1 | 1).write(&mut data)?;
            for word in network_repr.packed_data {
                word.write(&mut data)?;
            }
            match network_repr.palette {
                NetworkPalette::Single(id) => VarInt(i32::from(id)).write(&mut data)?,
                NetworkPalette::Indirect(palette) => {
                    VarInt(palette.len() as i32).write(&mut data)?;
                    for id in palette {
                        VarInt(i32::from(id)).write(&mut data)?;
                    }
                }
                NetworkPalette::Direct => (),
            }
            if chunk.section.min_y == NETHER_DIMENSION.min_y
                && index + 1 == NETHER_SECTIONS
                && biomes.len() > NETHER_SECTIONS
            {
                ceiling = Some(data.len());
            }
        }

        // A cached palette can supply the full frame and a Nether prefix from
        // the same byte allocation, with a different hash for each view.
        let full = blob(data);
        let restricted = ceiling.map(|end| {
            let bytes = full.1.slice(..end);
            (xxh64(&bytes, 0), bytes)
        });
        Ok([Some(full), restricted].into_iter().flatten().collect())
    })
}

impl CLevelChunk<'_> {
    pub fn encode_chunk(
        chunk: &ChunkData,
        dimension: i32,
        cache_enabled: bool,
        block_actors: &[NbtCompound],
    ) -> Result<EncodedChunk, Error> {
        let mut writer = Vec::new();
        let nether = dimension == bedrock_dimension_id(&NETHER_DIMENSION);

        VarInt(chunk.x).write(&mut writer)?;
        VarInt(chunk.z).write(&mut writer)?;

        VarInt(dimension).write(&mut writer)?;
        let block_blobs = chunk
            .section
            .block_sections
            .bedrock_blobs(|block_sections| {
                let sub_chunk_count = block_sections
                    .iter()
                    .rposition(|section| !section.has_only_air())
                    .map_or(0, |i| i + 1);
                let mut blobs = Vec::with_capacity(sub_chunk_count);
                let min_y_section = (chunk.section.min_y >> 4) as i8;
                for (i, block_palette) in block_sections.iter().take(sub_chunk_count).enumerate() {
                    let mut subchunk_buf = Vec::new();
                    // Version 9: [version:byte][num_storages:byte][sub_chunk_index:byte]
                    let y = (i as i8) + min_y_section;
                    let water_layer = block_palette.convert_be_water_network();
                    let num_storages = if water_layer.is_some() { 2 } else { 1 };
                    subchunk_buf.write_all(&[VERSION, num_storages, y as u8])?;

                    write_block_storage(&mut subchunk_buf, block_palette.convert_be_network())?;
                    if let Some(water_layer) = water_layer {
                        write_block_storage(&mut subchunk_buf, water_layer)?;
                    }

                    blobs.push(blob(subchunk_buf));
                }
                Ok(blobs)
            })?;
        let biome_blobs = encode_biomes(chunk)?;
        let (block_count, biomes) = if nether {
            (block_blobs.len().min(NETHER_SECTIONS), biome_blobs.last())
        } else {
            (block_blobs.len(), biome_blobs.first())
        };
        VarUInt(block_count as u32).write(&mut writer)?;
        false.write(&mut writer)?;
        cache_enabled.write(&mut writer)?;

        // Block entities can change independently of either palette.
        let actors = encode_block_actors(block_actors.iter().filter(|actor| {
            !nether
                || actor.get_int("y").is_none_or(|y| {
                    (NETHER_DIMENSION.min_y
                        ..NETHER_DIMENSION.min_y + NETHER_DIMENSION.logical_height)
                        .contains(&y)
                })
        }))?;
        let all_blobs = block_blobs.iter().take(block_count).chain(biomes);
        let mut blobs = Vec::new();
        if cache_enabled {
            blobs.extend(all_blobs.cloned());
            VarUInt(blobs.len() as u32).write(&mut writer)?;
            for (hash, _) in &blobs {
                writer.write_all(&hash.to_le_bytes())?;
            }
            VarUInt(
                u32::try_from(actors.len() + 1)
                    .map_err(|_| Error::other("oversized block actors"))?,
            )
            .write(&mut writer)?;
        } else {
            VarUInt(0).write(&mut writer)?;
            let size = all_blobs
                .clone()
                .map(|(_, bytes)| bytes.len())
                .sum::<usize>()
                + 1
                + actors.len();
            VarUInt(u32::try_from(size).map_err(|_| Error::other("oversized Bedrock chunk"))?)
                .write(&mut writer)?;
            for (_, bytes) in all_blobs {
                writer.write_all(bytes)?;
            }
        }
        writer.write_all(&[0])?;
        writer.write_all(&actors)?;
        // client_biome_update: false
        false.write(&mut writer)?;

        Ok((writer, blobs))
    }
}

impl PacketWrite for CLevelChunk<'_> {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        let (encoded, _) = Self::encode_chunk(
            self.chunk,
            self.dimension,
            self.cache_enabled,
            self.block_actors,
        )?;
        writer.write_all(&encoded)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use pumpkin_data::{Block, BlockState};
    use pumpkin_nbt::{Nbt, compound::NbtCompound, deserializer::NbtReadHelperBedrock};
    use sculith_world::chunk::{ChunkData, ChunkSections};

    use super::{CLevelChunk, NETHER_SECTIONS, VERSION};
    use sculith_protocol::{
        bedrock::client::level_chunk::CLevelChunkPayload,
        serial::{PacketReadSlice, PacketWrite},
    };

    fn read_var_uint(data: &[u8], offset: &mut usize) -> u32 {
        let mut value = 0;
        for shift in (0..35).step_by(7) {
            let byte = data[*offset];
            *offset += 1;
            value |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return value;
            }
        }
        panic!("VarUInt is too long");
    }

    fn read_var_int(data: &[u8], offset: &mut usize) -> i32 {
        let value = read_var_uint(data, offset);
        ((value >> 1) as i32) ^ -((value & 1) as i32)
    }

    fn skip_storage(data: &[u8], offset: &mut usize) -> Vec<u32> {
        let bits_per_entry = data[*offset] >> 1;
        *offset += 1;
        if bits_per_entry != 0 {
            let entries_per_word = 32 / usize::from(bits_per_entry);
            *offset += 4096usize.div_ceil(entries_per_word) * size_of::<u32>();
        }
        let palette_len = if bits_per_entry == 0 {
            1
        } else {
            read_var_int(data, offset) as usize
        };
        (0..palette_len)
            .map(|_| read_var_int(data, offset) as u32)
            .collect()
    }

    fn empty_chunk() -> ChunkData {
        ChunkData::empty(0, 0)
    }

    #[test]
    fn inline_terrain_shares_natural_frames_within_the_world_byte_budget() {
        let cache = super::InlineChunkCache::default();
        // Distinct loaded chunk identities exercise a single shard.
        let chunks = (0..6)
            .map(|_| std::sync::Arc::new(empty_chunk()))
            .collect::<Vec<_>>();
        let put = |i: usize, len: usize| {
            let chunk = &chunks[i];
            cache.insert(
                chunk,
                0,
                chunk.section.terrain_revision(),
                &bytes::Bytes::from(vec![0; len]),
            );
        };
        for i in 0..4 {
            put(i, 8192);
        }
        for chunk in &chunks[..4] {
            assert!(cache.get(chunk, 0).is_some());
        }
        put(4, 40 * 1024);
        assert!(cache.get(&chunks[0], 0).is_none());
        assert!(cache.get(&chunks[1], 0).is_some());
        assert_eq!(cache.get(&chunks[4], 0).unwrap().len(), 40 * 1024);
        put(5, 65537);
        assert!(cache.get(&chunks[5], 0).is_none());
        put(5, 65536);
        assert!(
            chunks[..5]
                .iter()
                .all(|chunk| cache.get(chunk, 0).is_none())
        );
        assert_eq!(cache.get(&chunks[5], 0).unwrap().len(), 65536);
    }

    #[test]
    fn inline_frame_tracks_both_palettes_and_the_loaded_chunk() {
        use crate::net::bedrock::frame_raw_packet;
        use sculith_protocol::packet::Packet;
        use std::sync::Arc;

        let cache = super::InlineChunkCache::default();
        let chunk = Arc::new(empty_chunk());
        let make_frame = || {
            let (bytes, _) = CLevelChunk::encode_chunk(&chunk, 0, false, &[]).unwrap();
            frame_raw_packet(CLevelChunk::PACKET_ID, &bytes).unwrap()
        };
        let frame = make_frame();
        let revision = chunk.section.terrain_revision();
        cache.insert(&chunk, 0, revision, &frame);
        assert_eq!(cache.get(&chunk, 0).unwrap().as_ptr(), frame.as_ptr());
        assert!(cache.get(&chunk, 1).is_none());
        assert!(cache.get(&Arc::new(empty_chunk()), 0).is_none());

        chunk.section.set_block_absolute_y(
            1,
            chunk.section.min_y,
            1,
            Block::STONE.default_state.id,
        );
        assert!(cache.get(&chunk, 0).is_none());
        cache.insert(&chunk, 0, revision, &frame);
        assert!(cache.get(&chunk, 0).is_none());

        let revised = make_frame();
        assert_ne!(revised, frame);
        cache.insert(&chunk, 0, chunk.section.terrain_revision(), &revised);
        chunk.section.biome_sections.write().unwrap()[0].set(0, 0, 0, 9);
        assert!(cache.get(&chunk, 0).is_none());
        assert_ne!(make_frame(), revised);
    }

    #[test]
    fn shared_palette_bytes_refresh_after_a_live_edit() {
        let chunk = ChunkData::empty(0, 0);
        let encode = |cache| CLevelChunk::encode_chunk(&chunk, 0, cache, &[]).unwrap();
        let (old_full, _) = encode(false);
        let (_, initial) = encode(true);
        let before = initial.last().unwrap().1.clone();
        chunk.section.set_block_absolute_y(
            0,
            chunk.section.min_y,
            0,
            Block::STONE.default_state.id,
        );
        let (new_full, _) = encode(false);
        let (_, next) = encode(true);
        assert_ne!(old_full, new_full);
        assert_eq!(before.as_ptr(), next.last().unwrap().1.as_ptr());
        let (_, again) = encode(true);
        assert_eq!(again[0].1.as_ptr(), next[0].1.as_ptr());
    }

    #[test]
    fn biomes_follow_subchunks_without_subchunk_headers() {
        let chunk = empty_chunk();
        let mut encoded = Vec::new();
        CLevelChunk {
            dimension: 0,
            cache_enabled: false,
            chunk: &chunk,
            block_actors: &[],
        }
        .write(&mut encoded)
        .unwrap();

        let mut offset = 0;
        for _ in 0..3 {
            read_var_uint(&encoded, &mut offset);
        }
        assert_eq!(read_var_uint(&encoded, &mut offset), 0); // All air -> no sub-chunks.
        assert_eq!(encoded[offset], 0); // No sub-chunk request limit.
        assert_eq!(encoded[offset + 1], 0); // Cache disabled.
        offset += 2;
        assert_eq!(read_var_uint(&encoded, &mut offset), 0);
        let raw_len = read_var_uint(&encoded, &mut offset) as usize;
        let raw = &encoded[offset..offset + raw_len];
        assert_eq!(encoded[offset + raw_len], 0); // client_biome_update: false
        assert_eq!(offset + raw_len + 1, encoded.len());

        let mut raw_offset = 0;
        for _ in 0..24 {
            assert_eq!(raw[raw_offset], 1); // No version/storage/Y prefix.
            raw_offset += 1;
            read_var_uint(raw, &mut raw_offset);
        }
        assert_eq!(raw[raw_offset], 0); // Border block count.
        assert_eq!(raw_offset + 1, raw.len());
    }

    #[test]
    fn nether_terrain_uses_only_the_native_height_even_after_an_end_send() {
        let mut chunk = empty_chunk();
        chunk.section = ChunkSections::new(16, 0);
        for y in [64, 128] {
            chunk
                .section
                .set_block_absolute_y(3, y, 3, Block::STONE.default_state.id);
        }
        let mut high_actor = NbtCompound::new();
        high_actor.put_int("y", 128);
        let actors = [high_actor];
        let (end, end_blobs) = CLevelChunk::encode_chunk(&chunk, 2, true, &actors).unwrap();
        let end = CLevelChunkPayload::read_slice(&mut end.as_slice()).unwrap();
        assert_eq!(end.sub_chunk_count.0, 9);
        assert!(end.chunk_data.len() > 1);

        for cache in [false, true] {
            let (body, blobs) = CLevelChunk::encode_chunk(&chunk, 1, cache, &actors).unwrap();
            let payload = CLevelChunkPayload::read_slice(&mut body.as_slice()).unwrap();
            assert_eq!(payload.sub_chunk_count.0 as usize, NETHER_SECTIONS);
            assert_eq!(payload.cache_enabled, cache);
            let (biomes, mut offset) = if cache {
                assert_eq!(payload.chunk_data, &[0]);
                assert_eq!(blobs.len(), NETHER_SECTIONS + 1);
                let (hash, bytes) = blobs.last().unwrap();
                assert_ne!(*hash, end_blobs.last().unwrap().0);
                assert_eq!(payload.blob_hashes.last(), Some(hash));
                (bytes.as_ref(), 0)
            } else {
                let mut offset = 0;
                for _ in 0..NETHER_SECTIONS {
                    assert_eq!(payload.chunk_data[offset], VERSION);
                    offset += 3;
                    skip_storage(payload.chunk_data, &mut offset);
                }
                (payload.chunk_data, offset)
            };
            for _ in 0..NETHER_SECTIONS {
                skip_storage(biomes, &mut offset);
            }
            assert_eq!(&biomes[offset..], if cache { &[][..] } else { &[0][..] });
        }
    }

    #[test]
    fn trailing_air_sub_chunks_are_not_sent() {
        let chunk = empty_chunk();
        chunk
            .section
            .set_block_absolute_y(0, 0, 0, Block::STONE.default_state.id);

        let (encoded, _) = CLevelChunk::encode_chunk(&chunk, 0, false, &[]).unwrap();
        let mut offset = 0;
        for _ in 0..3 {
            read_var_uint(&encoded, &mut offset);
        }
        // y = 0 is sub-chunk index 4 above min_y -64.
        assert_eq!(read_var_uint(&encoded, &mut offset), 5);
    }

    #[test]
    fn block_actor_nbt_follows_the_chunk_border_data() {
        let chunk = empty_chunk();
        let mut block_actor = NbtCompound::new();
        block_actor.put_string("id", "Chest".to_string());
        block_actor.put_int("x", 1);
        block_actor.put_int("y", 64);
        block_actor.put_int("z", 2);

        let (encoded, _) = CLevelChunk::encode_chunk(&chunk, 0, true, &[block_actor]).unwrap();
        let mut offset = 0;
        for _ in 0..3 {
            read_var_uint(&encoded, &mut offset);
        }
        read_var_uint(&encoded, &mut offset);
        offset += 2;
        let blob_count = read_var_uint(&encoded, &mut offset) as usize;
        offset += blob_count * size_of::<u64>();
        let raw_len = read_var_uint(&encoded, &mut offset) as usize;
        let raw = &encoded[offset..offset + raw_len];

        assert_eq!(raw[0], 0);
        let mut reader = NbtReadHelperBedrock::new(Cursor::new(&raw[1..]));
        let parsed = Nbt::read(&mut reader).unwrap();
        assert_eq!(parsed.get_string("id"), Some("Chest"));
        assert_eq!(parsed.get_int("x"), Some(1));
    }

    #[test]
    fn aquatic_blocks_use_a_secondary_water_storage() {
        let chunk = empty_chunk();
        chunk
            .section
            .set_block_absolute_y(0, -64, 0, Block::SEAGRASS.default_state.id);

        let (encoded, _) = CLevelChunk::encode_chunk(&chunk, 0, false, &[]).unwrap();
        let mut offset = 0;
        for _ in 0..4 {
            read_var_uint(&encoded, &mut offset);
        }
        offset += 2; // request limit and cache flag
        read_var_uint(&encoded, &mut offset); // blob count
        read_var_uint(&encoded, &mut offset); // raw payload length

        assert_eq!(&encoded[offset..offset + 3], &[VERSION, 2, (-4i8) as u8]);
        offset += 3;
        skip_storage(&encoded, &mut offset);
        let water_palette = skip_storage(&encoded, &mut offset);
        assert_eq!(
            water_palette,
            [
                BlockState::to_be_network_id(Block::AIR.default_state.id),
                BlockState::to_be_network_id(Block::WATER.default_state.id),
            ]
        );
    }
}
