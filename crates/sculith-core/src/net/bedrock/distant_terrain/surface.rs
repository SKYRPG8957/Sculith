use pumpkin_data::{Block, block_properties::is_air};
use sculith_world::chunk::ChunkData;

pub(super) const MATERIALS: [&str; 6] = ["earth", "green", "sand", "snow", "stone", "water"];

fn material(name: &str) -> u8 {
    if name.contains("water")
        || matches!(name, "kelp" | "kelp_plant" | "seagrass" | "tall_seagrass")
    {
        5
    } else if name.contains("snow") || name.contains("ice") {
        3
    } else if name.contains("sand") {
        2
    } else if name.contains("grass") || name.contains("leaves") || name.contains("moss") {
        1
    } else if name.contains("stone") || name.contains("ore") {
        4
    } else {
        0
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(super) struct Surface {
    pub y: i32,
    pub material: u8,
}

/// Four 8-block squares in Z-major order, followed by the 16-block square.
pub(super) type Summary = [Option<Surface>; 5];

fn representative<const N: usize>(cells: [Option<Surface>; N]) -> Option<Surface> {
    let mut heights = [0; N];
    let mut counts = [0u16; 6];
    for (i, cell) in cells.into_iter().enumerate() {
        let s = cell?;
        heights[i] = s.y;
        counts[usize::from(s.material)] += 1;
    }
    let (_, &mut y, _) = heights.select_nth_unstable((N - 1) / 2);
    let material = (0..counts.len()).max_by_key(|&i| (counts[i], std::cmp::Reverse(i)))?;
    Some(Surface {
        y,
        material: material as u8,
    })
}

pub(super) fn coarse(chunks: [Option<&Summary>; 4]) -> Option<Surface> {
    // Keep the terrain variation inside each child chunk at the coarsest level.
    representative(std::array::from_fn::<_, 16, _>(|i| {
        chunks[i / 4].and_then(|s| s[i % 4])
    }))
}

pub(super) fn summarize(chunk: &ChunkData) -> Summary {
    let sections = chunk
        .section
        .block_sections
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut columns = [None; 256];
    let mut remaining = columns.len();
    for (index, section) in sections.iter().enumerate().rev() {
        if section.has_only_air() {
            continue;
        }
        for (i, column) in columns.iter_mut().enumerate() {
            if column.is_some() {
                continue;
            }
            for y in (0..16).rev() {
                let state = section.get(i % 16, y, i / 16);
                if !is_air(state) {
                    *column = Some(Surface {
                        y: chunk.section.min_y + (index * 16 + y) as i32 + 1,
                        material: material(Block::from_state_id(state).name),
                    });
                    remaining -= 1;
                    break;
                }
            }
        }
        if remaining == 0 {
            break;
        }
    }
    drop(sections);
    let mut result = [None; 5];
    for (i, target) in result.iter_mut().take(4).enumerate() {
        let start = i % 2 * 8 + i / 2 * 128;
        *target = representative(std::array::from_fn::<_, 64, _>(|n| {
            columns[start + n / 8 * 16 + n % 8]
        }));
    }
    result[4] = representative(columns);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn negative_heights_patchy_surface_and_void() {
        let chunk = ChunkData::empty(-1, -1);
        assert_eq!(summarize(&chunk), [None; 5]);
        for z in 0..16 {
            for x in 0..16 {
                chunk
                    .section
                    .set_block_absolute_y(x, -29, z, Block::GRASS_BLOCK.default_state.id);
            }
        }
        let green = Some(Surface {
            y: -28,
            material: 1,
        });
        assert_eq!(summarize(&chunk), [green; 5]);
        for z in 0..8 {
            for x in 0..8 {
                chunk
                    .section
                    .set_block_absolute_y(x, -27, z, Block::STONE.default_state.id);
            }
        }
        assert_eq!(
            summarize(&chunk),
            [
                Some(Surface {
                    y: -26,
                    material: 4
                }),
                green,
                green,
                green,
                green
            ]
        );
    }
}
