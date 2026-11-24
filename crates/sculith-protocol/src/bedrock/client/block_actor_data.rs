// Last verified for v2169

use std::io::Error;

use pumpkin_macros::packet;
use pumpkin_nbt::compound::NbtCompound;
use sculith_util::math::position::BlockPos;

use crate::serial::{PacketRead, PacketReadSlice, PacketWrite};

/// Synchronizes the complete block-actor data for a block position.
#[derive(PacketWrite)]
#[packet(pumpkin_data::bedrock_packet::BLOCK_ACTOR_DATA)]
pub struct CBlockActorData {
    /// World coordinates of the block entity being updated.
    pub block_position: BlockPos,
    /// Network NBT compound containing all properties and state of the block entity.
    pub actor_data_tags: NbtCompound,
}

impl CBlockActorData {
    /// Creates a new block actor update packet for the given position and NBT data.
    #[must_use]
    pub const fn new(block_position: BlockPos, actor_data_tags: NbtCompound) -> Self {
        Self {
            block_position,
            actor_data_tags,
        }
    }
}

impl<'a> PacketReadSlice<'a> for CBlockActorData {
    fn read_slice(body: &mut &'a [u8]) -> Result<Self, Error> {
        Ok(Self {
            block_position: BlockPos::read(body)?,
            actor_data_tags: NbtCompound::read_slice(body)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use pumpkin_nbt::{Nbt, deserializer::NbtReadHelperBedrock};

    use super::*;
    use crate::{Packet, serial::PacketWrite};

    #[test]
    fn block_actor_data_uses_bedrock_network_nbt() {
        assert_eq!(<CBlockActorData as Packet>::PACKET_ID, 56);

        let mut data = NbtCompound::new();
        data.put_string("id", "Bed".to_string());
        data.put_byte("color", 11);

        let mut encoded = Vec::new();
        CBlockActorData {
            block_position: BlockPos::new(1, 64, -2),
            actor_data_tags: data,
        }
        .write(&mut encoded)
        .unwrap();

        assert_eq!(&encoded[..4], &[2, 128, 1, 3]);
        let mut reader = NbtReadHelperBedrock::new(Cursor::new(&encoded[4..]));
        let parsed = Nbt::read(&mut reader).unwrap();
        assert_eq!(parsed.get_string("id"), Some("Bed"));
        assert_eq!(parsed.get_byte("color"), Some(11));

        let mut body = encoded.as_slice();
        let decoded = CBlockActorData::read_slice(&mut body).unwrap();
        assert!(body.is_empty());
        assert_eq!(decoded.block_position, BlockPos::new(1, 64, -2));
        assert_eq!(decoded.actor_data_tags.get_string("id"), Some("Bed"));
    }
}
