use super::{
    region::{Atlas, Grid},
    surface::{Surface, coarse},
};
use pumpkin_util::math::vector2::Vector2;
use pumpkin_world::cylindrical_chunk_iterator::Cylindrical;
use std::{collections::BTreeMap, num::NonZero};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(super) struct View {
    pub center: Grid,
    pub radius: NonZero<u8>,
    pub budget: usize,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Tile {
    pub x: i32,
    pub z: i32,
    pub width: u8,
}

impl Tile {
    /// Regular game actors have i32 IDs. Use a disjoint range with one stable ID
    /// for every supported tile in Minecraft's +/-30 million block world.
    pub fn actor(self) -> u64 {
        fn signed(n: i32) -> u64 {
            u64::from(((n << 1) ^ (n >> 31)) as u32)
        }
        let level = u64::from(self.width.trailing_zeros() - 3);
        (1 << 58) | (level << 46) | (signed(self.x / 8) << 23) | signed(self.z / 8)
    }
}

impl View {
    pub fn regions(self) -> impl Iterator<Item = (Grid, i32)> {
        let reach = std::num::NonZeroI32::from(self.radius).get() * 4 + 2;
        let (x, z) = self.center;
        let (rx, rz) = (x.div_euclid(32), z.div_euclid(32));
        ((x - reach).div_euclid(32)..=(x + reach).div_euclid(32)).flat_map(move |a| {
            ((z - reach).div_euclid(32)..=(z + reach).div_euclid(32))
                .map(move |b| ((a, b), (a - rx).abs().max((b - rz).abs())))
        })
    }

    pub fn select(self, rows: &Atlas) -> BTreeMap<Tile, Surface> {
        let near = Cylindrical::new(Vector2::new(self.center.0, self.center.1), self.radius);
        let radius = std::num::NonZeroI32::from(self.radius).get();
        let mut choices = Vec::new();
        let reach = radius * 4 + 2;
        for (&(x, z), cells) in
            rows.range((self.center.0 - reach, i32::MIN)..=(self.center.0 + reach, i32::MAX))
        {
            if x.unsigned_abs() > 1_875_000 || z.unsigned_abs() > 1_875_000 {
                continue;
            }
            let px = x.div_euclid(2) * 2;
            let pz = z.div_euclid(2) * 2;
            // A parent and all four children use the same distance at tier boundaries.
            let d = (self.center.0 - px).abs().max((self.center.1 - pz).abs());
            if d > 4 * radius || near.is_within_distance(x, z) {
                continue;
            }
            if d <= radius * 3 / 2 {
                for (i, &s) in cells[..4].iter().enumerate() {
                    if let Some(s) = s {
                        choices.push((
                            d,
                            Tile {
                                x: x * 16 + (i as i32 % 2) * 8,
                                z: z * 16 + (i as i32 / 2) * 8,
                                width: 8,
                            },
                            s,
                        ));
                    }
                }
            } else if d <= radius * 5 / 2 {
                if let Some(s) = cells[4] {
                    choices.push((
                        d,
                        Tile {
                            x: x * 16,
                            z: z * 16,
                            width: 16,
                        },
                        s,
                    ));
                }
            } else if (x, z) == (px, pz)
                && let Some(s) = coarse(
                    [(px, pz), (px + 1, pz), (px, pz + 1), (px + 1, pz + 1)].map(|c| rows.get(&c)),
                )
            {
                choices.push((
                    d,
                    Tile {
                        x: x * 16,
                        z: z * 16,
                        width: 32,
                    },
                    s,
                ));
            }
        }
        if choices.len() > self.budget {
            choices
                .select_nth_unstable_by_key(self.budget, |&(distance, tile, _)| (distance, tile));
            choices.truncate(self.budget);
        }
        choices
            .into_iter()
            .take(self.budget)
            .map(|(_, key, s)| (key, s))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn same_terrain_selects_one_tier_outside_full_chunks() {
        let surface = Some(Surface { y: 64, material: 1 });
        let mut rows = Atlas::from([
            ((0, 0), [surface; 5]),
            ((1, 0), [surface; 5]),
            ((0, 1), [surface; 5]),
            ((1, 1), [surface; 5]),
        ]);
        let select = |rows: &Atlas, x, budget| {
            View {
                center: (x, 0),
                radius: NonZero::new(16).unwrap(),
                budget,
            }
            .select(rows)
        };
        assert!(select(&rows, 0, 32).is_empty());
        assert_eq!(
            select(&rows, 23, 4)
                .keys()
                .map(|t| t.width)
                .collect::<Vec<_>>(),
            vec![8; 4]
        );
        assert_eq!(
            select(&rows, 33, 32)
                .keys()
                .map(|t| t.width)
                .collect::<Vec<_>>(),
            vec![16; 4]
        );
        assert_eq!(
            select(&rows, 55, 32)
                .keys()
                .map(|t| t.width)
                .collect::<Vec<_>>(),
            vec![32]
        );
        rows.remove(&(1, 1));
        assert!(select(&rows, 55, 32).is_empty());
        assert!(select(&rows, 100, 32).is_empty());
        assert_ne!(
            Tile {
                x: -8,
                z: 0,
                width: 8
            }
            .actor(),
            Tile {
                x: 8,
                z: 0,
                width: 8
            }
            .actor()
        );
    }
}
