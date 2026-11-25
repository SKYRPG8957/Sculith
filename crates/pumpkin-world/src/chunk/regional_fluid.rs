use super::{BlockPalette, ChunkData, has_random_ticking_fluid, has_random_ticks};
use crate::tick::{ScheduledTick, TickPriority};
use pumpkin_data::{Block, BlockDirection, BlockStateId, fluid::Fluid};
use sculith_util::math::{position::BlockPos, vector3::Vector3};
use std::sync::{TryLockError, atomic::Ordering};

/// A scheduled water tick whose side effects fit the regional transaction.
pub enum DownwardWaterTick {
    Settled,
    Spread { state: BlockStateId, delay: u8 },
}

/// A water step whose changes and scheduled successors fit in one chunk.
pub enum RegionalWaterTick {
    Settled,
    SpreadDown { state: BlockStateId, delay: u8 },
    SpreadHorizontally { state: BlockStateId, delay: u8 },
}

impl From<DownwardWaterTick> for RegionalWaterTick {
    fn from(step: DownwardWaterTick) -> Self {
        match step {
            DownwardWaterTick::Settled => Self::Settled,
            DownwardWaterTick::Spread { state, delay } => Self::SpreadDown { state, delay },
        }
    }
}

impl ChunkData {
    pub(crate) fn try_apply_regional_water_ticks(
        &self,
        limit: usize,
        read_neighbor: &dyn Fn(BlockPos) -> Option<BlockStateId>,
        can_commit: &dyn Fn() -> bool,
        evaluate: impl Fn(
            BlockPos,
            BlockStateId,
            &dyn Fn(BlockPos) -> Option<BlockStateId>,
        ) -> Option<RegionalWaterTick>,
        next_order: impl Fn() -> u64,
    ) -> Option<Vec<(BlockPos, BlockStateId, BlockStateId)>> {
        let mut sections = self.section.block_sections.try_write_unmarked()?;
        let water = &Fluid::FLOWING_WATER;
        self.fluid_ticks
            .apply_and_schedule_ready_batch(limit, |ready| {
                let mut changes = Vec::new();
                let mut scheduled = Vec::new();
                for tick in ready {
                    if tick.value != water
                        || (
                            tick.position.chunk_position().x,
                            tick.position.chunk_position().y,
                        ) != (self.x, self.z)
                    {
                        return None;
                    }
                    let read = |position: BlockPos| {
                        let (chunk, relative) = position.chunk_and_chunk_relative_position();
                        if (chunk.x, chunk.y) != (self.x, self.z) {
                            return read_neighbor(position);
                        }
                        // Each ready tick sees the earlier decisions in timer order.
                        changes
                            .iter()
                            .rev()
                            .find_map(|&(at, _, state)| (at == position).then_some(state))
                            .or_else(|| self.section.read_block_from_palettes(&sections, relative))
                    };
                    let at = tick.position;
                    let (directions, state, delay) = match evaluate(at, read(at)?, &read)? {
                        RegionalWaterTick::Settled => continue,
                        RegionalWaterTick::SpreadDown { state, delay } => {
                            (&[BlockDirection::Down][..], state, delay)
                        }
                        RegionalWaterTick::SpreadHorizontally { state, delay } => (
                            &[
                                BlockDirection::North,
                                BlockDirection::South,
                                BlockDirection::West,
                                BlockDirection::East,
                            ][..],
                            state,
                            delay,
                        ),
                    };
                    if state.to_block() != &Block::WATER
                        || state.to_state().collision_shapes
                            != BlockStateId::AIR.to_state().collision_shapes
                        || has_random_ticks(state)
                        || has_random_ticking_fluid(state)
                    {
                        return None;
                    }
                    let followup = |position| ScheduledTick {
                        delay,
                        position,
                        value: water,
                        priority: TickPriority::Normal,
                    };
                    let targets = directions
                        .iter()
                        .map(|direction| at.offset(direction.to_offset()));
                    if targets.clone().any(|target| {
                        target.chunk_position() != at.chunk_position()
                            || read(target) != Some(BlockStateId::AIR)
                    }) {
                        return None;
                    }
                    for (index, target) in targets.enumerate() {
                        changes.push((target, BlockStateId::AIR, state));
                        scheduled.push(followup(target));
                        if index == 0 {
                            // The first placement wakes the origin after its own tick is queued.
                            scheduled.push(followup(at));
                        }
                    }
                }
                if !can_commit() {
                    return None;
                }

                if !changes.is_empty() {
                    self.write_water_changes(&mut sections, &changes)?;
                }
                self.dirty.store(true, Ordering::Relaxed);
                Some((
                    changes,
                    scheduled
                        .into_iter()
                        .map(|tick| (tick, next_order()))
                        .collect(),
                ))
            })
    }

    fn write_water_changes(
        &self,
        sections: &mut [BlockPalette],
        changes: &[(BlockPos, BlockStateId, BlockStateId)],
    ) -> Option<()> {
        // Other heightmap users can read palettes while holding their
        // lock. Never wait on that lock with a palette held.
        let mut heightmap = match self.heightmap.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        self.section.block_sections.mark_changed();
        for &(position, _, state) in changes {
            let relative = position.chunk_relative_position();
            let y = (relative.y - self.section.min_y) as usize;
            sections[y / BlockPalette::SIZE].set(
                relative.x as usize,
                y % BlockPalette::SIZE,
                relative.z as usize,
                state,
            );
            heightmap.update_for_block(relative, state.to_state(), self.section.min_y, |y| {
                self.section
                    .read_block_from_palettes(sections, Vector3::new(relative.x, y, relative.z))
                    .unwrap_or(BlockStateId::AIR)
                    .to_state()
            });
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_data::fluid::{Falling, FlowingWaterLikeFluidProperties, FluidProperties, Level};
    use std::cell::Cell;

    #[test]
    fn saves_a_water_step_with_both_follow_up_timers() {
        let water = &Fluid::FLOWING_WATER;
        let source = BlockPos::new(3, 64, 3);
        let below = source.down();
        let falling = FlowingWaterLikeFluidProperties {
            level: Level::L8,
            falling: Falling::True,
        }
        .to_state_id(water);
        let still = FlowingWaterLikeFluidProperties {
            level: Level::L8,
            falling: Falling::False,
        }
        .to_state_id(water);
        let chunk = ChunkData::empty(0, 0);
        chunk.set_block_absolute_y(3, 64, 3, still);
        chunk.fluid_ticks.schedule_tick(
            &ScheduledTick {
                position: source,
                value: water,
                delay: 0,
                priority: TickPriority::Normal,
            },
            0,
        );
        chunk.fluid_ticks.advance();
        let order = Cell::new(0);
        let next_order = || {
            order.set(order.get() + 1);
            order.get()
        };
        let apply = || {
            chunk.try_apply_regional_water_ticks(
                4,
                &|_| None,
                &|| true,
                |at, _, read| {
                    if read(at.down())? == falling {
                        assert_eq!(read(at.down().down()), Some(falling));
                        return Some(RegionalWaterTick::Settled);
                    }
                    Some(RegionalWaterTick::SpreadDown {
                        state: falling,
                        delay: 5,
                    })
                },
                next_order,
            )
        };

        let busy_heightmap = chunk.heightmap.lock().unwrap();
        assert_eq!(apply(), None);
        drop(busy_heightmap);
        assert_eq!(
            chunk.section.get_block_absolute_y(3, 63, 3),
            Some(BlockStateId::AIR)
        );
        assert!(chunk.fluid_ticks.is_scheduled(source, water));
        assert_eq!(order.get(), 0);
        assert_eq!(apply(), Some(vec![(below, BlockStateId::AIR, falling)]));

        for bytes in [chunk.internal_to_bytes(), chunk.internal_to_pump_bytes()] {
            let saved = ChunkData::internal_from_bytes(&bytes, source.chunk_position()).unwrap();
            assert_eq!(saved.section.get_block_absolute_y(3, 63, 3), Some(falling));
            let timers = saved.fluid_ticks.to_vec();
            assert_eq!(timers.len(), 2);
            assert!(
                timers
                    .iter()
                    .all(|tick| tick.value == water && tick.delay == 5)
            );
            assert!(timers.iter().any(|tick| tick.position == source));
            assert!(timers.iter().any(|tick| tick.position == below));
        }

        for _ in 0..=5 {
            chunk.fluid_ticks.advance();
        }
        assert_eq!(
            apply(),
            Some(vec![(below.down(), BlockStateId::AIR, falling)])
        );
        assert!(!chunk.fluid_ticks.is_scheduled(source, water));
        assert!(chunk.fluid_ticks.is_scheduled(below, water));
        assert!(chunk.fluid_ticks.is_scheduled(below.down(), water));
    }

    #[test]
    fn saves_all_four_horizontal_placements_and_their_follow_up_ticks() {
        let water = &Fluid::FLOWING_WATER;
        let source = BlockPos::new(8, 64, 8);
        let spread = FlowingWaterLikeFluidProperties {
            level: Level::L7,
            falling: Falling::False,
        }
        .to_state_id(water);
        let chunk = ChunkData::empty(0, 0);
        let still = FlowingWaterLikeFluidProperties {
            level: Level::L8,
            falling: Falling::False,
        }
        .to_state_id(water);
        chunk.set_block_absolute_y(8, 64, 8, still);
        chunk.fluid_ticks.schedule_tick(
            &ScheduledTick {
                position: source,
                value: water,
                delay: 0,
                priority: TickPriority::Normal,
            },
            0,
        );
        chunk.fluid_ticks.advance();
        let order = Cell::new(0);
        let changes = chunk
            .try_apply_regional_water_ticks(
                4,
                &|_| None,
                &|| true,
                |_, _, _| {
                    Some(RegionalWaterTick::SpreadHorizontally {
                        state: spread,
                        delay: 5,
                    })
                },
                || {
                    order.set(order.get() + 1);
                    order.get()
                },
            )
            .unwrap();
        assert_eq!(changes.len(), 4);
        let positions = BlockDirection::horizontal().map(|side| source.offset(side.to_offset()));
        for bytes in [chunk.internal_to_bytes(), chunk.internal_to_pump_bytes()] {
            let saved = ChunkData::internal_from_bytes(&bytes, source.chunk_position()).unwrap();
            for position in positions {
                let relative = position.chunk_relative_position();
                assert_eq!(
                    saved.section.get_block_absolute_y(
                        relative.x as usize,
                        relative.y,
                        relative.z as usize
                    ),
                    Some(spread)
                );
            }
            let timers = saved.fluid_ticks.to_vec();
            assert_eq!(timers.len(), 5);
            for pos in positions.into_iter().chain(std::iter::once(source)) {
                assert!(timers.iter().any(|tick| {
                    tick.position == pos && tick.value == water && tick.delay == 5
                }));
            }
        }
    }
}
