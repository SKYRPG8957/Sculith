use std::io::{Error, ErrorKind, Read, Write};

use pumpkin_macros::packet;
use pumpkin_util::math::{position::BlockPos, vector2::Vector2};

use crate::{
    codec::{var_int::VarInt, var_uint::VarUInt},
    serial::{PacketRead, PacketWrite},
};

/// Sent by the server to update the center anchor position and radius around which chunks remain loaded on the client.
#[packet(pumpkin_data::bedrock_packet::NETWORK_CHUNK_PUBLISHER_UPDATE)]
pub struct CNetworkChunkPublisherUpdate {
    // https://mojang.github.io/bedrock-protocol-docs/html/NetworkChunkPublisherUpdatePacket.html
    /// Center block position of the loaded area.
    pub pos_for_view: BlockPos,
    // Is in blocks, not chunks!
    /// Radius around the view position (measured in blocks, typically chunk radius * 16).
    pub new_radius: VarUInt,
    // TODO
    /// List of chunk coordinates saved in the client's cache.
    pub server_build_chunk_list: Vec<Vector2<i32>>,
}

impl CNetworkChunkPublisherUpdate {
    #[must_use]
    pub const fn new(pos_for_view: BlockPos, new_radius: u32) -> Self {
        Self {
            pos_for_view,
            new_radius: VarUInt(new_radius),
            server_build_chunk_list: Vec::new(),
        }
    }
}

impl PacketWrite for CNetworkChunkPublisherUpdate {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.pos_for_view.write(writer)?;
        self.new_radius.write(writer)?;

        (self.server_build_chunk_list.len() as u32).write(writer)?;
        for chunk in &self.server_build_chunk_list {
            VarInt(chunk.x).write(writer)?;
            VarInt(chunk.y).write(writer)?;
        }
        Ok(())
    }
}

impl PacketRead for CNetworkChunkPublisherUpdate {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let pos_for_view = BlockPos::read(reader)?;
        let new_radius = VarUInt::read(reader)?;
        let count = u32::read(reader)?;
        if count > 9216 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "too many saved chunk positions",
            ));
        }
        let mut server_build_chunk_list = Vec::new();
        for _ in 0..count {
            server_build_chunk_list.push(Vector2 {
                x: VarInt::read(reader)?.0,
                y: VarInt::read(reader)?.0,
            });
        }
        Ok(Self {
            pos_for_view,
            new_radius,
            server_build_chunk_list,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_saved_chunk_positions_as_signed_varints() {
        let bytes = [1, 128, 1, 4, 128, 2, 1, 0, 0, 0, 3, 6];
        let mut body = bytes.as_slice();
        let packet = CNetworkChunkPublisherUpdate::read(&mut body).unwrap();
        assert!(body.is_empty());
        assert_eq!(packet.pos_for_view, BlockPos::new(-1, 64, 2));
        assert_eq!(packet.new_radius, VarUInt(256));
        assert_eq!(packet.server_build_chunk_list, [Vector2 { x: -2, y: 3 }]);
        let mut written = Vec::new();
        packet.write(&mut written).unwrap();
        assert_eq!(written, bytes);
    }
}
