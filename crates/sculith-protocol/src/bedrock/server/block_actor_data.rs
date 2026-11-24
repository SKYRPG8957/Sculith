use std::io::Error;

use pumpkin_macros::packet;
use pumpkin_nbt::compound::NbtCompound;
use sculith_util::math::position::BlockPos;

use crate::{
    bedrock::client::CBlockActorData,
    serial::{PacketRead, PacketReadSlice},
};

/// Synchronizes block-actor updates from the client, such as edited sign text.
#[derive(PacketRead)]
#[packet(pumpkin_data::bedrock_packet::BLOCK_ACTOR_DATA)]
pub struct SBlockActorData {
    /// World coordinates of the block entity being updated.
    pub block_position: BlockPos,
    /// Network NBT compound containing all properties and state of the block entity.
    pub actor_data_tags: NbtCompound,
}

impl<'a> PacketReadSlice<'a> for SBlockActorData {
    fn read_slice(body: &mut &'a [u8]) -> Result<Self, Error> {
        let decoded = CBlockActorData::read_slice(body)?;
        Ok(Self {
            block_position: decoded.block_position,
            actor_data_tags: decoded.actor_data_tags,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::{
        Packet,
        bedrock::client::CBlockActorData,
        serial::{PacketRead, PacketWrite},
    };

    #[test]
    fn block_actor_data_roundtrip() {
        assert_eq!(<SBlockActorData as Packet>::PACKET_ID, 56);

        let mut data = NbtCompound::new();
        data.put_string("id", "Sign".to_string());
        data.put_string("Text", "Hello\nWorld".to_string());

        let mut encoded = Vec::new();
        CBlockActorData {
            block_position: BlockPos::new(10, 64, -20),
            actor_data_tags: data,
        }
        .write(&mut encoded)
        .unwrap();

        let mut cursor = Cursor::new(&encoded);
        let parsed = SBlockActorData::read(&mut cursor).unwrap();

        assert_eq!(parsed.block_position, BlockPos::new(10, 64, -20));
        assert_eq!(parsed.actor_data_tags.get_string("id"), Some("Sign"));
        assert_eq!(
            parsed.actor_data_tags.get_string("Text"),
            Some("Hello\nWorld")
        );
    }
}
