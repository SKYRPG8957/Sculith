use std::io::{Error, ErrorKind, Write};

use pumpkin_macros::packet;

use crate::{
    MAX_PACKET_DATA_SIZE,
    codec::{var_int::VarInt, var_uint::VarUInt},
    serial::{PacketRead, PacketReadSlice, PacketWrite},
};

/// Encoded column, optional cache references and chunk payload sent to a Bedrock client.
#[packet(pumpkin_data::bedrock_packet::LEVEL_CHUNK)]
pub struct CLevelChunkPayload<'a> {
    pub chunk_x: VarInt,
    pub chunk_z: VarInt,
    pub dimension: VarInt,
    pub sub_chunk_count: VarUInt,
    pub sub_chunk_limit: Option<VarInt>,
    pub cache_enabled: bool,
    pub blob_hashes: Vec<u64>,
    pub chunk_data: &'a [u8],
    pub client_biome_update: bool,
}

impl PacketWrite for CLevelChunkPayload<'_> {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        if self.chunk_data.len() > MAX_PACKET_DATA_SIZE || self.sub_chunk_count.0 > 64 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Bedrock chunk is too large",
            ));
        }
        self.chunk_x.write(writer)?;
        self.chunk_z.write(writer)?;
        self.dimension.write(writer)?;
        self.sub_chunk_count.write(writer)?;
        self.sub_chunk_limit.write(writer)?;
        self.cache_enabled.write(writer)?;
        VarUInt(self.blob_hashes.len() as u32).write(writer)?;
        self.blob_hashes.write(writer)?;
        VarUInt(self.chunk_data.len() as u32).write(writer)?;
        writer.write_all(self.chunk_data)?;
        self.client_biome_update.write(writer)
    }
}

impl<'a> PacketReadSlice<'a> for CLevelChunkPayload<'a> {
    fn read_slice(body: &mut &'a [u8]) -> Result<Self, Error> {
        let chunk_x = VarInt::read(body)?;
        let chunk_z = VarInt::read(body)?;
        let dimension = VarInt::read(body)?;
        let sub_chunk_count = VarUInt::read(body)?;
        if sub_chunk_count.0 > 64 {
            return Err(Error::new(ErrorKind::InvalidData, "too many sub-chunks"));
        }
        let sub_chunk_limit = Option::<VarInt>::read(body)?;
        let cache_enabled = bool::read(body)?;
        let blob_hashes = Vec::<u64>::read(body)?;
        let len = VarUInt::read(body)?.0 as usize;
        if len > MAX_PACKET_DATA_SIZE {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "Bedrock chunk payload is too large",
            ));
        }
        let (chunk_data, rest) = body.split_at_checked(len).ok_or(Error::new(
            ErrorKind::UnexpectedEof,
            "incomplete Bedrock chunk payload",
        ))?;
        *body = rest;
        let client_biome_update = bool::read(body)?;
        Ok(Self {
            chunk_x,
            chunk_z,
            dimension,
            sub_chunk_count,
            sub_chunk_limit,
            cache_enabled,
            blob_hashes,
            chunk_data,
            client_biome_update,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_chunk_header_and_borrows_the_serialized_payload() {
        let packet = CLevelChunkPayload {
            chunk_x: VarInt(-11),
            chunk_z: VarInt(6),
            dimension: VarInt(1),
            sub_chunk_count: VarUInt(2),
            sub_chunk_limit: Some(VarInt(12)),
            cache_enabled: true,
            blob_hashes: vec![u64::MAX],
            chunk_data: b"chunk contents",
            client_biome_update: true,
        };
        let mut encoded = Vec::new();
        packet.write(&mut encoded).unwrap();
        let mut body = encoded.as_slice();
        let decoded = CLevelChunkPayload::read_slice(&mut body).unwrap();
        assert!(body.is_empty());
        assert_eq!(decoded.chunk_x, packet.chunk_x);
        assert_eq!(decoded.chunk_z, packet.chunk_z);
        assert_eq!(decoded.sub_chunk_limit, packet.sub_chunk_limit);
        assert_eq!(decoded.blob_hashes, packet.blob_hashes);
        assert_eq!(decoded.chunk_data, packet.chunk_data);
        assert!(decoded.client_biome_update);
    }
}
