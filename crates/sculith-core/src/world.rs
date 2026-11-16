//! Bounded, protocol-independent world operations for trusted server plugins.
use serde::{Deserialize, Serialize};

pub const MAX_WORLD_BLOCKS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldPosition {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldBlock {
    pub position: WorldPosition,
    pub block: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionInfo {
    pub id: u64,
    pub revision: u64,
    pub min: WorldPosition,
    pub max_exclusive: WorldPosition,
}

/// Shared with the original player-only BlocksChanged wire contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockChange {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorldOperation {
    /// An empty read discovers the current region and its revision.
    Read { positions: Vec<WorldPosition> },
    /// Compare-and-set the complete bounded batch against a region revision.
    Edit {
        region_id: u64,
        expected_revision: u64,
        blocks: Vec<WorldBlock>,
    },
}
impl WorldOperation {
    pub fn units(&self) -> usize {
        match self {
            Self::Read { positions } => positions.len(),
            Self::Edit { blocks, .. } => blocks.len(),
        }
        .max(1)
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Read { positions } if positions.len() <= MAX_WORLD_BLOCKS => Ok(()),
            Self::Edit { blocks, .. } if !blocks.is_empty() && blocks.len() <= MAX_WORLD_BLOCKS => {
                for block in blocks {
                    crate::validate_resource_id(&block.block)
                        .map_err(|_| "invalid block identifier")?;
                }
                Ok(())
            }
            _ => Err("world request exceeds its block limit or has an empty edit"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorldError {
    InvalidRequest,
    OutsideRegion,
    UnknownRegion,
    UnsupportedBlock,
    RevisionConflict { expected: u64, actual: u64 },
    RateLimited,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorldResult {
    Snapshot {
        region: RegionInfo,
        blocks: Vec<WorldBlock>,
    },
    Updated {
        region: RegionInfo,
        previous_revision: u64,
        changes: Vec<BlockChange>,
    },
    Rejected {
        reason: WorldError,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorldActor {
    Player { player_id: u64 },
    Plugin { name: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn world_shapes_bound_work_and_identifiers_before_dispatch() {
        let position = WorldPosition { x: 0, y: 4, z: 0 };
        assert!(
            WorldOperation::Read { positions: vec![] }
                .validate()
                .is_ok()
        );
        assert_eq!(WorldOperation::Read { positions: vec![] }.units(), 1);
        assert!(
            WorldOperation::Read {
                positions: vec![position; 64]
            }
            .validate()
            .is_ok()
        );
        assert!(
            WorldOperation::Read {
                positions: vec![position; 65]
            }
            .validate()
            .is_err()
        );
        let block = WorldBlock {
            position,
            block: "minecraft:stone".into(),
        };
        assert!(
            WorldOperation::Edit {
                region_id: 0,
                expected_revision: 0,
                blocks: vec![block.clone(); 64]
            }
            .validate()
            .is_ok()
        );
        assert!(
            WorldOperation::Edit {
                region_id: 0,
                expected_revision: 0,
                blocks: vec![block; 65]
            }
            .validate()
            .is_err()
        );
        assert!(
            WorldOperation::Edit {
                region_id: 0,
                expected_revision: 0,
                blocks: vec![WorldBlock {
                    position,
                    block: "invalid\nblock".into()
                }]
            }
            .validate()
            .is_err()
        );
    }
}
