//! Java protocol 777 section/palette/heightmap/light layout, same owner snapshots.
//! Unlike older versions: fluid counts, fixed-size palette words and byte bitsets.
use super::{codec, registry, write_varint};
use anyhow::Result;
use sculith_world::{BlockKind, ChunkSnapshot, MIN_Y, SECTION_BLOCKS};

pub fn encode(snapshot: &ChunkSnapshot) -> Result<Vec<u8>> {
    let registry = registry::get();
    let mut packet = codec::packet(0x2e);
    let position = snapshot.position();
    packet.extend_from_slice(&position.x.to_be_bytes());
    packet.extend_from_slice(&position.z.to_be_bytes());
    let heights = heights(snapshot);
    write_varint(2, &mut packet);
    for kind in [1, 4] {
        // WORLD_SURFACE and MOTION_BLOCKING, not the WG variants.
        write_varint(kind, &mut packet);
        write_varint(37, &mut packet); // 256 values, seven non-spanning 9-bit values per u64.
        for entries in heights.chunks(7) {
            let word = entries
                .iter()
                .enumerate()
                .fold(0u64, |word, (i, &v)| word | (u64::from(v) << (i * 9)));
            packet.extend_from_slice(&word.to_be_bytes());
        }
    }
    let mut sections = Vec::new();
    for (_, section) in snapshot.sections() {
        let count = SECTION_BLOCKS as u16 - section.count(BlockKind::Air);
        sections.extend_from_slice(&count.to_be_bytes());
        sections.extend_from_slice(&0u16.to_be_bytes()); // No fluids in the current core registry.
        if section.stats().uniform {
            sections.push(0);
            write_varint(registry.state(section.get(0, 0, 0)?), &mut sections);
        } else {
            let mut palette = Vec::with_capacity(BlockKind::ALL.len());
            let mut reverse = [u8::MAX; BlockKind::ALL.len()];
            let mut words = [0u64; SECTION_BLOCKS / 16];
            // Internal traversal is already Java's X-fast, then Z, then Y order.
            // Pack directly instead of retaining 4096 heap-allocated u64 indices.
            for (offset, block) in section.blocks().enumerate() {
                let slot = &mut reverse[block as usize];
                if *slot == u8::MAX {
                    *slot = palette.len() as u8;
                    palette.push(block);
                }
                words[offset / 16] |= u64::from(*slot) << ((offset % 16) * 4);
            }
            sections.push(4); // Local block palette minimum.
            write_varint(palette.len() as i32, &mut sections);
            for block in palette {
                write_varint(registry.state(block), &mut sections);
            }
            // Protocol 777 derives the word count from bits and section size.
            for word in words {
                sections.extend_from_slice(&word.to_be_bytes());
            }
        }
        sections.push(0); // Single-valued plains biome palette, no data words.
        write_varint(registry.biome, &mut sections);
    }
    write_varint(sections.len() as i32, &mut packet);
    packet.extend_from_slice(&sections);
    write_varint(0, &mut packet); // Block entities.
    light(&heights, &mut packet);
    Ok(packet)
}

fn light(heights: &[u16; 256], packet: &mut Vec<u8>) {
    // All 24 sections and the two boundary sections. Explicit full lighting is
    // bounded to 26*2048 bytes; no fluids/emissive/transparent core blocks exist.
    let all = ((1u32 << 26) - 1).to_le_bytes();
    for bytes in [&all[..], &[][..], &[][..], &all[..]] {
        write_varint(bytes.len() as i32, packet);
        packet.extend_from_slice(bytes);
    }
    write_varint(26, packet);
    let min = i32::from(*heights.iter().min().expect("256 columns")) + MIN_Y;
    let max = i32::from(*heights.iter().max().expect("256 columns")) + MIN_Y;
    for section_y in -5..=20 {
        write_varint(2048, packet);
        let base = section_y * 16;
        if base + 15 < min || base >= max {
            let value = if base >= max { 0xff } else { 0 };
            packet.resize(packet.len() + 2048, value);
            continue;
        }
        for y in 0..16 {
            let world_y = base + y;
            if world_y < min || world_y >= max {
                let value = if world_y >= max { 0xff } else { 0 };
                packet.resize(packet.len() + 128, value);
                continue;
            }
            for z in 0..16usize {
                for x in (0..16usize).step_by(2) {
                    let value = |x| {
                        if world_y >= i32::from(heights[x + 16 * z]) + MIN_Y {
                            15u8
                        } else {
                            0
                        }
                    };
                    packet.push(value(x) | value(x + 1) << 4);
                }
            }
        }
    }
    write_varint(0, packet); // Block light arrays are all explicitly empty.
}

pub fn light_update(snapshot: &ChunkSnapshot) -> Result<Vec<u8>> {
    // Reuse the same deterministic sky calculation after an edit.
    let heights = heights(snapshot);
    let mut packet = codec::packet(0x31);
    write_varint(snapshot.position().x, &mut packet);
    write_varint(snapshot.position().z, &mut packet);
    light(&heights, &mut packet);
    Ok(packet)
}

fn heights(snapshot: &ChunkSnapshot) -> [u16; 256] {
    snapshot.non_air_heights().map(|y| (y - MIN_Y) as u16)
}
