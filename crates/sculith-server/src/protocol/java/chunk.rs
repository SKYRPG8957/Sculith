//! Java protocol 777 section/palette/heightmap/light layout, same owner snapshots.
//! Unlike older versions: fluid counts, fixed-size palette words and byte bitsets.
use super::{codec, registry, write_varint};
use anyhow::Result;
use sculith_world::{BlockKind, ChunkSnapshot, MAX_Y, MIN_Y};

pub fn encode(snapshot: &ChunkSnapshot) -> Result<Vec<u8>> {
    let registry = registry::get();
    let mut packet = codec::packet(0x2e);
    let position = snapshot.position();
    packet.extend_from_slice(&position.x.to_be_bytes());
    packet.extend_from_slice(&position.z.to_be_bytes());
    let mut heights = [0u16; 256];
    for z in 0..16u8 {
        for x in 0..16u8 {
            heights[x as usize + 16 * z as usize] = (MIN_Y..=MAX_Y)
                .rev()
                .find(|&y| snapshot.get(x, y, z).is_ok_and(|b| b != BlockKind::Air))
                .map_or(0, |y| (y + 1 - MIN_Y) as u16);
        }
    }
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
        let count = section.blocks().filter(|b| *b != BlockKind::Air).count() as u16;
        sections.extend_from_slice(&count.to_be_bytes());
        sections.extend_from_slice(&0u16.to_be_bytes()); // No fluids in the current core registry.
        if section.stats().uniform {
            sections.push(0);
            write_varint(registry.state(section.get(0, 0, 0)?), &mut sections);
        } else {
            let mut palette = Vec::new();
            let mut indices = Vec::with_capacity(4096);
            // Java is X-fast, then Z, then Y; never reuse Bedrock's wire order.
            for y in 0..16 {
                for z in 0..16 {
                    for x in 0..16 {
                        let block = section.get(x, y, z)?;
                        let index = match palette.iter().position(|b| *b == block) {
                            Some(index) => index,
                            None => {
                                palette.push(block);
                                palette.len() - 1
                            }
                        };
                        indices.push(index as u64);
                    }
                }
            }
            sections.push(4); // Local block palette minimum.
            write_varint(palette.len() as i32, &mut sections);
            for block in palette {
                write_varint(registry.state(block), &mut sections);
            }
            // Protocol 777 derives the word count from bits and section size.
            for entries in indices.chunks(16) {
                let word = entries
                    .iter()
                    .enumerate()
                    .fold(0u64, |word, (i, &v)| word | (v << (i * 4)));
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
    for section_y in -5..=20 {
        write_varint(2048, packet);
        for y in 0..16 {
            for z in 0..16usize {
                for x in (0..16usize).step_by(2) {
                    let world_y = section_y * 16 + y;
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
    let mut heights = [0u16; 256];
    for z in 0..16u8 {
        for x in 0..16u8 {
            heights[x as usize + 16 * z as usize] = (MIN_Y..=MAX_Y)
                .rev()
                .find(|&y| snapshot.get(x, y, z).is_ok_and(|b| b != BlockKind::Air))
                .map_or(0, |y| (y + 1 - MIN_Y) as u16);
        }
    }
    let mut packet = codec::packet(0x31);
    write_varint(snapshot.position().x, &mut packet);
    write_varint(snapshot.position().z, &mut packet);
    light(&heights, &mut packet);
    Ok(packet)
}
