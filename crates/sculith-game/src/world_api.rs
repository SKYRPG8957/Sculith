//! Owner-only world access. Protocol tasks and plugin workers never own Region.
use crate::{Budget, GameState};
use sculith_core::world::{
    BlockChange, RegionInfo, WorldBlock, WorldError, WorldOperation, WorldPosition, WorldResult,
};
use sculith_world::{BlockEdit, BlockKind, BlockPos, CommittedBatch};
use std::time::Instant;

const UNITS_PER_SECOND: f64 = 4096.0;
const BURST: f64 = 8192.0;

impl GameState {
    pub fn plugin_world(
        &mut self,
        operation: &WorldOperation,
        now: Instant,
    ) -> (WorldResult, Option<CommittedBatch>) {
        match self.apply_world(operation, now) {
            Ok(result) => result,
            Err(reason) => (WorldResult::Rejected { reason }, None),
        }
    }
    fn apply_world(
        &mut self,
        operation: &WorldOperation,
        now: Instant,
    ) -> Result<(WorldResult, Option<CommittedBatch>), WorldError> {
        operation
            .validate()
            .map_err(|_| WorldError::InvalidRequest)?;
        let available = match self.world_budget {
            Some(budget) => budget
                .available(now, UNITS_PER_SECOND, BURST)
                .ok_or(WorldError::RateLimited)?,
            None => BURST,
        };
        let cost = operation.units() as f64;
        if cost > available {
            return Err(WorldError::RateLimited);
        }
        self.world_budget = Some(Budget {
            remaining: available - cost,
            updated: now,
        });
        match operation {
            WorldOperation::Read { positions } => {
                let blocks = positions
                    .iter()
                    .map(|position| {
                        let block = self
                            .region
                            .get_block(convert(*position))
                            .map_err(map_error)?;
                        Ok(WorldBlock {
                            position: *position,
                            block: block_name(block).into(),
                        })
                    })
                    .collect::<Result<Vec<_>, WorldError>>()?;
                Ok((
                    WorldResult::Snapshot {
                        region: self.region_info(),
                        blocks,
                    },
                    None,
                ))
            }
            WorldOperation::Edit {
                region_id,
                expected_revision,
                blocks,
            } => {
                if *region_id != self.region.id().0 {
                    return Err(WorldError::UnknownRegion);
                }
                let edits = blocks
                    .iter()
                    .map(|block| {
                        Ok(BlockEdit {
                            position: convert(block.position),
                            block: block_kind(&block.block).ok_or(WorldError::UnsupportedBlock)?,
                        })
                    })
                    .collect::<Result<Vec<_>, WorldError>>()?;
                let batch = self
                    .region
                    .commit(*expected_revision, &edits)
                    .map_err(map_error)?;
                let result = WorldResult::Updated {
                    region: self.region_info(),
                    previous_revision: batch.previous_revision,
                    changes: describe_changes(&batch),
                };
                let notification = (!batch.changes.is_empty()).then_some(batch);
                Ok((result, notification))
            }
        }
    }
    fn region_info(&self) -> RegionInfo {
        let bounds = self.region.bounds();
        RegionInfo {
            id: self.region.id().0,
            revision: self.region.revision(),
            min: WorldPosition {
                x: bounds.min().x * 16,
                y: sculith_world::MIN_Y,
                z: bounds.min().z * 16,
            },
            max_exclusive: WorldPosition {
                x: bounds.max_exclusive().x * 16,
                y: sculith_world::MAX_Y + 1,
                z: bounds.max_exclusive().z * 16,
            },
        }
    }
}
fn convert(position: WorldPosition) -> BlockPos {
    BlockPos {
        x: position.x,
        y: position.y,
        z: position.z,
    }
}
fn map_error(error: sculith_world::WorldError) -> WorldError {
    use sculith_world::WorldError as E;
    match error {
        E::OutsideRegion { .. } | E::InvalidBlockPosition(_) | E::InvalidChunkPosition(_) => {
            WorldError::OutsideRegion
        }
        E::RevisionConflict { expected, actual } => {
            WorldError::RevisionConflict { expected, actual }
        }
        E::DuplicateEdit(_) | E::TooManyEdits(_) => WorldError::InvalidRequest,
        _ => WorldError::Unavailable,
    }
}
fn block_kind(name: &str) -> Option<BlockKind> {
    Some(match name {
        "minecraft:air" => BlockKind::Air,
        "minecraft:stone" => BlockKind::Stone,
        "minecraft:dirt" => BlockKind::Dirt,
        "minecraft:grass_block" => BlockKind::Grass,
        "minecraft:bedrock" => BlockKind::Bedrock,
        _ => return None,
    })
}
fn block_name(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Air => "minecraft:air",
        BlockKind::Stone => "minecraft:stone",
        BlockKind::Dirt => "minecraft:dirt",
        BlockKind::Grass => "minecraft:grass_block",
        BlockKind::Bedrock => "minecraft:bedrock",
    }
}
pub fn describe_changes(batch: &CommittedBatch) -> Vec<BlockChange> {
    batch
        .changes
        .iter()
        .map(|c| BlockChange {
            x: c.position.x,
            y: c.position.y,
            z: c.position.z,
            before: block_name(c.before).into(),
            after: block_name(c.after).into(),
        })
        .collect()
}

#[cfg(test)]
mod tests;
