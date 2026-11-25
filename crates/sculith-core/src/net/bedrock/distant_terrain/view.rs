use super::{
    region::{Atlas, Grid},
    surface::{Surface, coarse},
};
use pumpkin_world::cylindrical_chunk_iterator::Cylindrical;
use sculith_util::math::vector2::Vector2;
use std::{collections::BTreeMap, num::NonZero};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(super) struct View {
    pub center: Grid,
    pub radius: NonZero<u8>,
    full_chunk_radius: NonZero<u8>,
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
    pub(super) const fn from_chunk_view(
        chunks: Cylindrical,
        radius: NonZero<u8>,
        budget: usize,
    ) -> Self {
        Self {
            center: (chunks.center.x, chunks.center.y),
            radius,
            full_chunk_radius: chunks.view_distance,
            budget,
        }
    }

    pub(super) const fn chunk_view(self) -> Cylindrical {
        Cylindrical::new(
            Vector2::new(self.center.0, self.center.1),
            self.full_chunk_radius,
        )
    }

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
        let near = self.chunk_view();
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
        self.limit_tiles(choices)
    }

    fn limit_tiles(self, mut choices: Vec<(i32, Tile, Surface)>) -> BTreeMap<Tile, Surface> {
        if choices.len() <= self.budget {
            return choices.into_iter().map(|(_, tile, s)| (tile, s)).collect();
        }
        // Reserve a visible sample in each quadrant at each available scale.
        // A continuous near ring must not crowd every distant tile off the client.
        let mut coverage = [None; 12];
        for &(distance, tile, surface) in &choices {
            let tier = tile.width.trailing_zeros() as usize - 3;
            let east = usize::from(tile.x.div_euclid(16) >= self.center.0);
            let south = usize::from(tile.z.div_euclid(16) >= self.center.1);
            let entry = &mut coverage[4 * tier + 2 * east + south];
            if entry.is_none_or(|(d, t, _)| (distance, tile) < (d, t)) {
                *entry = Some((distance, tile, surface));
            }
        }
        let mut result: BTreeMap<_, _> = coverage
            .into_iter()
            .flatten()
            .take(self.budget)
            .map(|(_, tile, surface)| (tile, surface))
            .collect();
        if result.len() < self.budget {
            let (nearest, _, _) =
                choices.select_nth_unstable_by_key(self.budget, |&(d, tile, _)| (d, tile));
            nearest.sort_unstable_by_key(|&(d, tile, _)| (d, tile));
            for &(.., tile, surface) in nearest.iter() {
                result.insert(tile, surface);
                if result.len() == self.budget {
                    break;
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distant_tiles_leave_room_for_the_published_buffered_view() {
        let surface = Some(Surface { y: 64, material: 1 });
        let rows = Atlas::from([
            ((6, 0), [surface; 5]),
            ((9, 0), [surface; 5]),
            ((18, 0), [surface; 5]),
        ]);
        let radius = NonZero::new(4).unwrap();
        let requested = Cylindrical::new(Vector2::new(0, 0), radius);
        let published = Cylindrical::new(Vector2::new(1, 0), NonZero::new(6).unwrap());
        assert!(
            View::from_chunk_view(requested, radius, 32)
                .select(&rows)
                .keys()
                .any(|tile| tile.x.div_euclid(16) == 6)
        );
        let selected = View::from_chunk_view(published, radius, 32).select(&rows);
        assert!(!selected.is_empty());
        assert!(selected.keys().all(|tile| tile.x.div_euclid(16) == 9));
    }

    #[test]
    fn continuous_terrain_covers_all_quadrants_and_scales_at_the_lowest_client_budget() {
        let surface = Some(Surface { y: 64, material: 1 });
        let rows: Atlas = (-64..=64)
            .flat_map(|x| (-64..=64).map(move |z| ((x, z), [surface; 5])))
            .collect();
        let view = View {
            center: (0, 0),
            radius: NonZero::new(16).unwrap(),
            full_chunk_radius: NonZero::new(16).unwrap(),
            budget: 12,
        };
        let tiles = view.select(&rows);
        let areas: std::collections::BTreeSet<_> = tiles
            .keys()
            .map(|t| (t.width, t.x >= 0, t.z >= 0))
            .collect();
        assert_eq!(tiles.len(), 12);
        assert_eq!(areas.len(), 12);
        assert_eq!(tiles, view.select(&rows));
    }

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
                full_chunk_radius: NonZero::new(16).unwrap(),
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
