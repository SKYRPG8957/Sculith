//! Opt-in host observations. Disabled hooks add no world-mailbox request and do
//! not fill the game-event bus. These flags do not change simulation semantics.
pub const PACKETS: u8 = 1;
pub const MOVEMENT: u8 = 2;
pub const EQUIPMENT: u8 = 4;
pub const INVENTORY: u8 = 8;
pub const ALL: u8 = PACKETS | MOVEMENT | EQUIPMENT | INVENTORY;
