use std::io::{Error, Write};

use bytes::Bytes;
use pumpkin_macros::packet;
use pumpkin_nbt::NbtCompound;

use crate::serial::{PacketRead, PacketWrite};

pub const VANILLA_ACTOR_IDENTIFIERS: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/bedrock/entity_identifiers.dat"
));

/// Registers actor identifiers before their first spawn packet.
#[packet(pumpkin_data::bedrock_packet::AVAILABLE_ACTOR_IDENTIFIERS)]
pub struct CAvailableActorIdentifiers {
    pub identifiers: Bytes,
}

impl PacketWrite for CAvailableActorIdentifiers {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        // This packet contains network NBT directly, without a byte-array length.
        writer.write_all(&self.identifiers)
    }
}

/// Parsed actor identifier registry carried by `CAvailableActorIdentifiers`.
#[derive(PacketRead)]
#[packet(pumpkin_data::bedrock_packet::AVAILABLE_ACTOR_IDENTIFIERS)]
pub struct CAvailableActorIdentifiersPayload {
    pub identifiers: NbtCompound,
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
        let mut body = encoded.as_slice();
        let decoded = CAvailableActorIdentifiersPayload::read(&mut body).unwrap();
        assert!(body.is_empty());
        assert_eq!(decoded.identifiers.get_list("idlist").unwrap().len(), 0);
    }
}
