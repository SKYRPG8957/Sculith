use pumpkin_macros::packet;

use crate::serial::PacketRead;

/// Sent when a player picks a targeted entity.
#[derive(Debug, PacketRead)]
#[packet(pumpkin_data::bedrock_packet::ACTOR_PICK_REQUEST)]
pub struct SActorPickRequest {
    /// Unlike most actor IDs, this field is a fixed little-endian integer.
    pub entity_unique_id: i64,
    pub max_slots: u8,
    pub with_data: bool,
}
