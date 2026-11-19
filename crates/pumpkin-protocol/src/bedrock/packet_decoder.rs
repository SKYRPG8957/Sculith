use std::io::{Cursor, Read};

use bytes::Bytes;

use crate::{
    CompressionThreshold, MAX_PACKET_DATA_SIZE, PacketDecodeError, RawPacket,
    bedrock::BEDROCK_GAME_PACKET, codec::var_uint::VarUInt, ser::ReadingError,
};

/// Decodes incoming Bedrock game packets and decompresses batched network frames.
#[derive(Clone, Copy)]
pub struct BedrockBatchDecoder {
    compression: Option<CompressionThreshold>,
    prefixed_compression: bool,
}

impl Default for BedrockBatchDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl BedrockBatchDecoder {
    /// Creates a new uncompressed batch decoder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            compression: None,
            prefixed_compression: true,
        }
    }

    /// Sets the decompression threshold for incoming batch frames.
    pub const fn set_compression(&mut self, threshold: CompressionThreshold) {
        self.compression = Some(threshold);
    }

    /// Sets whether compressed batch frames include the 1-byte compression method prefix (`1.20.60+`).
    pub const fn set_prefixed_compression(&mut self, prefixed: bool) {
        self.prefixed_compression = prefixed;
    }

    /// Validates the frame header, handles decompression if active, and extracts the raw batch payload.
    #[expect(
        clippy::unused_async,
        reason = "Preserves async API compatibility for select loops"
    )]
    pub async fn get_packet_payload(
        &mut self,
        full_packet: Vec<u8>,
    ) -> Result<Vec<u8>, PacketDecodeError> {
        let header = full_packet
            .first()
            .copied()
            .ok_or_else(|| PacketDecodeError::MalformedLength("Empty packet".into()))?;
        if header != BEDROCK_GAME_PACKET {
            return Err(PacketDecodeError::MalformedLength(format!(
                "Missing 0xfe header (found 0x{header:02x})"
            )));
        }
        self.decode_nethernet_batch(Bytes::from(full_packet).slice(1..))
            .map(Vec::from)
    }

    /// Decodes `NetherNet`'s markerless batch, retaining the original buffer for raw packets.
    pub fn decode_nethernet_batch(&self, payload: Bytes) -> Result<Bytes, PacketDecodeError> {
        let plain = match (self.compression, self.prefixed_compression) {
            (None, _) | (Some(0), false) => payload,
            (Some(_), false) => return Self::inflate(&payload),
            (Some(_), true) => match payload.first() {
                Some(0x00) => return Self::inflate(&payload[1..]),
                Some(0xff) => payload.slice(1..),
                Some(method) => {
                    return Err(PacketDecodeError::FailedDecompression(format!(
                        "Unsupported compression method: 0x{method:02x}"
                    )));
                }
                None => {
                    return Err(PacketDecodeError::MalformedLength(
                        "Missing Bedrock compression method".into(),
                    ));
                }
            },
        };
        if plain.len() > MAX_PACKET_DATA_SIZE {
            return Err(PacketDecodeError::TooLong);
        }
        Ok(plain)
    }

    fn inflate(payload: &[u8]) -> Result<Bytes, PacketDecodeError> {
        let mut decoder =
            flate2::read::DeflateDecoder::new(payload).take(MAX_PACKET_DATA_SIZE as u64 + 1);
        let mut decompressed = Vec::new();
        decoder
            .read_to_end(&mut decompressed)
            .map_err(|error| PacketDecodeError::FailedDecompression(error.to_string()))?;
        if decompressed.len() > MAX_PACKET_DATA_SIZE {
            return Err(PacketDecodeError::TooLong);
        }
        Ok(decompressed.into())
    }

    /// Reads the next game packet header and payload from a decompressed batch frame reader.
    pub fn get_game_packet(
        &self,
        decompressed_reader: &mut Cursor<impl AsRef<[u8]>>,
    ) -> Result<RawPacket, PacketDecodeError> {
        let packet_len = VarUInt::decode(decompressed_reader).map_err(|err| match err {
            ReadingError::CleanEOF(_) => PacketDecodeError::ConnectionClosed,
            err => PacketDecodeError::MalformedLength(err.to_string()),
        })?;
        let packet_len = packet_len.0 as usize;
        if packet_len == 0 {
            return Err(PacketDecodeError::MalformedLength(
                "Bedrock game packet length is zero".into(),
            ));
        }
        if packet_len > MAX_PACKET_DATA_SIZE {
            return Err(PacketDecodeError::TooLong);
        }

        let var_header = VarUInt::decode(decompressed_reader)?;
        let header = var_header.0 & 0x3FFF;
        let gamepacket_id = (header & 0x3FF) as u16;

        let header_size = var_header.written_size();
        if packet_len < header_size {
            return Err(PacketDecodeError::MalformedLength(format!(
                "Bedrock game packet length {packet_len} is smaller than header size {header_size}"
            )));
        }

        let payload_len = packet_len - header_size;
        let remaining = decompressed_reader
            .get_ref()
            .as_ref()
            .len()
            .saturating_sub(decompressed_reader.position() as usize);
        if payload_len > remaining {
            return Err(PacketDecodeError::MalformedLength(format!(
                "Bedrock game packet payload length {payload_len} exceeds remaining batch bytes {remaining}"
            )));
        }

        let mut payload = vec![0; payload_len].into_boxed_slice();
        decompressed_reader
            .read_exact(&mut payload)
            .map_err(|err| PacketDecodeError::FailedDecompression(err.to_string()))?;

        Ok(RawPacket {
            id: i32::from(gamepacket_id),
            payload: payload.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use crate::bedrock::{SubClient, packet_encoder::BedrockBatchEncoder};

    use super::*;

    #[test]
    fn decodes_nethernet_batches_on_both_sides_of_threshold() {
        let mut decoder = BedrockBatchDecoder::new();
        decoder.set_compression(256);
        let mut encoder = BedrockBatchEncoder::new();
        encoder.set_compression((256, 4));

        for (size, method) in [(32, 0xff), (300, 0x00)] {
            let sent = vec![0x2a; size];
            let mut wire = Vec::new();
            encoder
                .write_game_packet(1, SubClient::Main, SubClient::Main, &sent, &mut wire)
                .expect("encode test frame");
            assert_eq!(wire[1], method);
            let transport = Bytes::from(wire).slice(1..);
            let plain = decoder
                .decode_nethernet_batch(transport.clone())
                .expect("decode markerless frame");
            if method == 0xff {
                assert_eq!(plain.as_ptr(), transport[1..].as_ptr());
            }
            let packet = decoder
                .get_game_packet(&mut Cursor::new(plain))
                .expect("read test frame");
            assert_eq!(packet.id, 1);
            assert_eq!(packet.payload.as_ref(), sent);
        }
    }

    #[test]
    fn decodes_payload_larger_than_two_mib() {
        const PAYLOAD_LEN: usize = 2 * 1024 * 1024 + 1;
        let payload = vec![0x2a; PAYLOAD_LEN];
        let mut wire_buf = Vec::new();
        let network_encoder = BedrockBatchEncoder::new();
        network_encoder
            .write_game_packet(
                0x01,
                SubClient::Main,
                SubClient::Main,
                &payload,
                &mut wire_buf,
            )
            .expect("encode Bedrock game packet");

        let mut cursor = Cursor::new(wire_buf[1..].to_vec());
        let decoder = BedrockBatchDecoder::new();

        let packet = decoder
            .get_game_packet(&mut cursor)
            .expect("decode Bedrock game packet");

        assert_eq!(packet.id, 0x01);
        assert_eq!(packet.payload.len(), PAYLOAD_LEN);
        assert_eq!(packet.payload.as_ref(), payload.as_slice());
    }
}
