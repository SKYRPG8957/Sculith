use pumpkin_macros::packet;

use crate::{
    codec::var_int::VarInt,
    serial::{PacketRead, PacketWrite},
};

/// Starts a client-side cooldown for an item category.
#[derive(PacketRead, PacketWrite)]
#[packet(pumpkin_data::bedrock_packet::CLIENT_START_ITEM_COOLDOWN)]
pub struct CStartItemCooldown {
    pub category: String,
    pub duration: VarInt,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_and_duration_match_bedrock_2225() {
        let mut input = &b"\x0bender_pearl\xa0\x01"[..];
        let packet = CStartItemCooldown::read(&mut input).unwrap();
        assert!(input.is_empty());
        assert_eq!(packet.category, "ender_pearl");
        assert_eq!(packet.duration.0, 80);
        let mut encoded = Vec::new();
        packet.write(&mut encoded).unwrap();
        assert_eq!(encoded, b"\x0bender_pearl\xa0\x01");
    }
}
