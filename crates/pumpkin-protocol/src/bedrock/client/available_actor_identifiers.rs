use std::io::{Error, Write};

use bytes::Bytes;
use pumpkin_macros::packet;

use crate::serial::PacketWrite;

pub const VANILLA_ACTOR_IDENTIFIERS: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/bedrock/entity_identifiers.dat"
));

/// Registers actor identifiers before their first spawn packet.
#[packet(119)]
pub struct CAvailableActorIdentifiers {
    pub identifiers: Bytes,
}

impl PacketWrite for CAvailableActorIdentifiers {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        // This packet contains network NBT directly, without a byte-array length.
        writer.write_all(&self.identifiers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_network_nbt_without_a_length_prefix() {
        let empty_registry = b"\x0a\x00\x09\x06idlist\x0a\x00\x00";
        let packet = CAvailableActorIdentifiers {
            identifiers: Bytes::from_static(empty_registry),
        };
        let mut encoded = Vec::new();
        packet.write(&mut encoded).unwrap();
        assert_eq!(encoded, empty_registry);
    }
}
