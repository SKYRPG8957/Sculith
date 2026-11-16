//! Edition-independent inventory requests and private plugin responses.
use serde::{Deserialize, Serialize};

pub const SLOT_COUNT: usize = 36;
pub const STACK_LIMIT: u16 = 64;
pub const MAX_GIVE_COUNT: u16 = SLOT_COUNT as u16 * STACK_LIMIT;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InventoryOperation {
    Get,
    Give { item: String, count: u16 },
}

impl InventoryOperation {
    pub fn validate(&self) -> Result<(), String> {
        if let Self::Give { item, count } = self {
            super::validate_resource_id(item)?;
            if !(1..=MAX_GIVE_COUNT).contains(count) {
                return Err("inventory give count must be between 1 and 2304".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryItem {
    pub item: String,
    pub count: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryView {
    /// Owner slots 0..8 are the hotbar, 9..35 the remaining player inventory.
    pub slots: Vec<Option<InventoryItem>>,
    pub selected_slot: u8,
    pub cursor: Option<InventoryItem>,
    pub created_output: Option<InventoryItem>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryError {
    PlayerUnavailable,
    InvalidRequest,
    UnsupportedItem,
    Full,
    RateLimited,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InventoryResult {
    /// Get returns current state; Give returns state after the atomic grant.
    Snapshot {
        inventory: InventoryView,
    },
    Rejected {
        reason: InventoryError,
    },
}
