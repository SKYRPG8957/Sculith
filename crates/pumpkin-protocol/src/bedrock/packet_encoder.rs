use std::{
    cell::Cell,
    io::{Error, ErrorKind, Write},
};

use flate2::{Compression, write::DeflateEncoder};

use crate::{
    CompressionLevel, CompressionThreshold,
    bedrock::{BEDROCK_GAME_PACKET, SubClient},
    codec::var_uint::VarUInt,
    ser::NetworkWriteExt,
};

/// Encodes outgoing Bedrock game packets and batches them into compressed frames.
#[derive(Clone, Copy)]
pub struct BedrockBatchEncoder {
    // compression and compression threshold
    compression: Option<(CompressionThreshold, CompressionLevel)>,
    prefixed_compression: bool,
}

impl Default for BedrockBatchEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl BedrockBatchEncoder {
    /// Creates a new uncompressed batch encoder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            compression: None,
            prefixed_compression: true,
        }
    }

    /// Sets the compression threshold and Deflate level for batched frames.
    pub const fn set_compression(
        &mut self,
        compression_info: (CompressionThreshold, CompressionLevel),
    ) {
        self.compression = Some(compression_info);
    }

    /// Sets whether compressed batch frames include the 1-byte compression method prefix (`1.20.60+`).
    pub const fn set_prefixed_compression(&mut self, prefixed: bool) {
        self.prefixed_compression = prefixed;
    }

    /// Encodes a raw packet payload with the 14-bit Bedrock game packet header into a batch frame.
    pub fn write_game_packet(
        &self,
        packet_id: u16,
        sub_client_sender: SubClient,
        sub_client_target: SubClient,
        packet_payload: &[u8],
        writer: impl Write,
    ) -> Result<(), Error> {
        self.write_game_packets(
            &[(packet_id, packet_payload)],
            sub_client_sender,
            sub_client_target,
            writer,
        )
    }

    /// Applies the negotiated threshold to the complete length-prefixed batch.
    pub fn write_game_packets<P: AsRef<[u8]>>(
        &self,
        packets: &[(u16, P)],
        sender: SubClient,
        target: SubClient,
        mut writer: impl Write,
    ) -> Result<(), Error> {
        let sub_clients = ((sender as u32) << 10) | ((target as u32) << 12);
        let header = |id: u16| VarUInt((u32::from(id) | sub_clients) & 0x3fff);
        let mut size = 0;
        for (id, data) in packets {
            let len =
                u32::try_from(data.as_ref().len() + header(*id).written_size()).map_err(|_| {
                    Error::new(ErrorKind::InvalidInput, "Bedrock game packet is too long")
                })?;
            size += len as usize + VarUInt(len).written_size();
        }
        let write_frames = |out: &mut dyn Write| -> Result<(), Error> {
            for (id, data) in packets {
                let head = header(*id);
                let bytes = data.as_ref();
                let mut field = [0_u8; 10];
                let mut dst = &mut field[..];
                let len = VarUInt((head.written_size() + bytes.len()) as u32);
                dst.write_var_uint(&len).map_err(Error::other)?;
                dst.write_var_uint(&head).map_err(Error::other)?;
                out.write_all(&field[..len.written_size() + head.written_size()])?;
                out.write_all(bytes)?;
            }
            Ok(())
        };
        writer.write_all(&[BEDROCK_GAME_PACKET])?;
        let Some((threshold, level)) = self.compression else {
            return write_frames(&mut writer);
        };
        let compress = threshold > 0 && (size >= threshold || !self.prefixed_compression);
        if self.prefixed_compression {
            writer.write_all(&[if compress { 0 } else { 0xff }])?;
        }
        if !compress {
            return write_frames(&mut writer);
        }
        if size > 16 * 1024 {
            let mut deflater = DeflateEncoder::new(writer, Compression::new(level.min(9)));
            write_frames(&mut deflater)?;
            deflater.finish()?;
            return Ok(());
        }
        // Raw DEFLATE streams must restart for every game batch. Reuse the
        // compressor workspace and coalesce small writes from a busy crowd.
        thread_local! {
            static DEFLATER: Cell<Option<BatchDeflater>> = const { Cell::new(None) };
        }
        DEFLATER.with(|cache| {
            let mut d = cache
                .take()
                .filter(|d| d.level == level)
                .unwrap_or_else(|| BatchDeflater {
                    level,
                    encoder: DeflateEncoder::new(Vec::new(), Compression::new(level.min(9))),
                    scratch: Vec::new(),
                });
            d.scratch.clear();
            write_frames(&mut d.scratch)?;
            d.encoder.write_all(&d.scratch)?;
            let mut completed = d.encoder.reset(Vec::new())?;
            let result = writer.write_all(&completed);
            completed.clear();
            *d.encoder.get_mut() = completed;
            cache.set(Some(d));
            result
        })
    }

    /// Encodes a typed Bedrock clientbound packet into a batch frame and writes it.
    pub fn write_packet<P: crate::BClientPacket + ?Sized>(
        &self,
        packet: &P,
        writer: impl Write,
    ) -> Result<(), Error> {
        let mut packet_payload = Vec::new();
        packet.write_packet(&mut packet_payload)?;
        self.write_game_packet(
            P::PACKET_ID as u16,
            SubClient::Main,
            SubClient::Main,
            &packet_payload,
            writer,
        )
    }

    /// Serializes a typed Bedrock clientbound packet into an in-memory byte buffer.
    pub fn serialize_packet<P: crate::BClientPacket + ?Sized>(
        &self,
        packet: &P,
    ) -> Result<bytes::Bytes, Error> {
        let mut buf = Vec::new();
        self.write_packet(packet, &mut buf)?;
        Ok(buf.into())
    }
}

struct BatchDeflater {
    level: CompressionLevel,
    encoder: DeflateEncoder<Vec<u8>>,
    scratch: Vec<u8>,
}

/// Encodes a typed Bedrock clientbound packet with default encoder settings.
pub fn write_packet<P: crate::BClientPacket + ?Sized>(
    packet: &P,
    writer: impl Write,
) -> Result<(), Error> {
    BedrockBatchEncoder::new().write_packet(packet, writer)
}

/// Serializes a typed Bedrock clientbound packet to bytes with default encoder settings.
pub fn serialize_packet<P: crate::BClientPacket + ?Sized>(
    packet: &P,
) -> Result<bytes::Bytes, Error> {
    BedrockBatchEncoder::new().serialize_packet(packet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bedrock::packet_decoder::BedrockBatchDecoder;
    use std::io::Cursor;

    #[tokio::test]
    async fn bedrock_compression_cycle() -> Result<(), Box<dyn std::error::Error>> {
        let mut encoder = BedrockBatchEncoder::new();
        encoder.set_compression((256, 6));

        let packet_id = 1;
        let payload = b"Hello Bedrock Compression!";
        let mut encoded_buf = Vec::new();

        encoder.write_game_packet(
            packet_id,
            SubClient::Main,
            SubClient::Main,
            payload,
            &mut encoded_buf,
        )?;

        let mut decoder = BedrockBatchDecoder::new();
        decoder.set_compression(256);

        let decompressed_payload = decoder.get_packet_payload(encoded_buf).await?;
        let mut cursor = Cursor::new(decompressed_payload);
        let raw_packet = decoder.get_game_packet(&mut cursor)?;

        assert_eq!(raw_packet.id, packet_id as i32);
        assert_eq!(raw_packet.payload.as_ref(), payload);
        Ok(())
    }

    #[tokio::test]
    async fn bedrock_unprefixed_compression_cycle() -> Result<(), Box<dyn std::error::Error>> {
        let mut encoder = BedrockBatchEncoder::new();
        encoder.set_compression((256, 6));
        encoder.set_prefixed_compression(false);

        let packet_id = 300;
        let payload = b"Legacy pre-1.20.60 Bedrock batch";
        let mut encoded_buf = Vec::new();
        encoder.write_game_packet(
            packet_id,
            SubClient::Main,
            SubClient::Main,
            payload,
            &mut encoded_buf,
        )?;

        let mut decoder = BedrockBatchDecoder::new();
        decoder.set_compression(256);
        decoder.set_prefixed_compression(false);

        let decompressed_payload = decoder.get_packet_payload(encoded_buf).await?;
        let mut cursor = Cursor::new(decompressed_payload);
        let raw_packet = decoder.get_game_packet(&mut cursor)?;

        assert_eq!(raw_packet.id, i32::from(packet_id));
        assert_eq!(raw_packet.payload.as_ref(), payload);
        Ok(())
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;
    use flate2::read::DeflateDecoder;
    use std::io::Read;

    #[test]
    fn threshold_covers_all_packets_including_their_headers() {
        let mut enc = BedrockBatchEncoder::new();
        enc.set_compression((8, 4));
        let packets = [(7, &[1, 2][..]), (9, &[3, 4][..])];
        let mut wire = Vec::new();
        enc.write_game_packets(&packets[..1], SubClient::Main, SubClient::Main, &mut wire)
            .unwrap();
        assert_eq!(wire, [0xfe, 0xff, 3, 7, 1, 2]);
        for _ in 0..2 {
            wire.clear();
            enc.write_game_packets(&packets, SubClient::Main, SubClient::Main, &mut wire)
                .unwrap();
            assert_eq!(&wire[..2], &[0xfe, 0]);
            let mut raw = Vec::new();
            DeflateDecoder::new(&wire[2..])
                .read_to_end(&mut raw)
                .unwrap();
            assert_eq!(raw, [3, 7, 1, 2, 3, 9, 3, 4]);
        }
        enc.set_compression((0, 4));
        wire.clear();
        enc.write_game_packets(&packets, SubClient::Main, SubClient::Main, &mut wire)
            .unwrap();
        assert_eq!(wire, [0xfe, 0xff, 3, 7, 1, 2, 3, 9, 3, 4]);
    }
}
