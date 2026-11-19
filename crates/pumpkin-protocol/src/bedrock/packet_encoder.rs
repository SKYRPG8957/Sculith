use std::{
    cell::Cell,
    hash::{BuildHasher, RandomState},
    io::{Error, ErrorKind, Write},
    sync::{LazyLock, Mutex},
};

use bytes::Bytes;
use flate2::{Compression, write::DeflateEncoder};
use zlib_rs::{Deflate, DeflateConfig, DeflateFlush, Status, compress_bound};

use crate::{
    CompressionLevel, CompressionThreshold,
    bedrock::{BEDROCK_GAME_PACKET, SubClient},
    codec::var_uint::VarUInt,
    ser::NetworkWriteExt,
};

// Match the history window to the largest batch retained in the thread-local workspace.
const BATCH_WINDOW_BITS: i32 = 14;

/// Encodes outgoing Bedrock game packets and batches them into compressed frames.
#[derive(Clone, Copy, PartialEq, Eq)]
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
        writer: impl Write,
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
                let mut field = [0u8; 10];
                let mut dst = &mut field[..];
                let len = VarUInt((head.written_size() + bytes.len()) as u32);
                dst.write_var_uint(&len).map_err(Error::other)?;
                dst.write_var_uint(&head).map_err(Error::other)?;
                out.write_all(&field[..len.written_size() + head.written_size()])?;
                out.write_all(bytes)?;
            }
            Ok(())
        };
        self.write_batch(size, false, write_frames, writer)
    }

    /// Writes length prefixes around packets that already contain a canonical Bedrock
    /// header. Returns false without writing if a header needs decoding and normalizing.
    pub fn try_write_framed_game_packets<'a>(
        &self,
        packets: impl Iterator<Item = &'a [u8]> + Clone,
        writer: impl Write,
    ) -> Result<bool, Error> {
        use pumpkin_data::bedrock_packet::{
            LEVEL_CHUNK, MOVE_ACTOR_DELTA, MOVE_PLAYER, SET_ACTOR_MOTION,
        };
        let mut size = 0;
        let mut world_frames = true;
        for packet in packets.clone() {
            // A canonical 14-bit header occupies one byte, or two with a nonzero high part.
            if !matches!(packet, [0..=0x7f, ..] | [0x80..=0xff, 1..=0x7f, ..]) {
                return Ok(false);
            }
            world_frames &= matches!(
                i32::from(packet[0]),
                LEVEL_CHUNK | MOVE_PLAYER | MOVE_ACTOR_DELTA | SET_ACTOR_MOTION
            );
            let len = u32::try_from(packet.len()).map_err(|_| {
                Error::new(ErrorKind::InvalidInput, "Bedrock game packet is too long")
            })?;
            size += packet.len() + VarUInt(len).written_size();
        }
        self.write_batch(
            size,
            world_frames,
            |out| {
                let mut prefix = [0; 5];
                for packet in packets {
                    let len = VarUInt(packet.len() as u32);
                    prefix
                        .as_mut_slice()
                        .write_var_uint(&len)
                        .map_err(Error::other)?;
                    out.write_all(&prefix[..len.written_size()])?;
                    out.write_all(packet)?;
                }
                Ok(())
            },
            writer,
        )?;
        Ok(true)
    }

    fn write_batch(
        &self,
        size: usize,
        share: bool,
        write_frames: impl FnOnce(&mut dyn Write) -> Result<(), Error>,
        mut writer: impl Write,
    ) -> Result<(), Error> {
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
        if size > 1 << BATCH_WINDOW_BITS {
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
                    encoder: Deflate::new_with_config(DeflateConfig {
                        window_bits: -BATCH_WINDOW_BITS,
                        level: level.min(9) as i32,
                        mem_level: 6,
                        ..DeflateConfig::default()
                    }),
                    scratch: Vec::new(),
                    output: Vec::new(),
                });
            d.scratch.clear();
            write_frames(&mut d.scratch)?;
            // The negotiated prefix and threshold were applied above. The raw
            // DEFLATE suffix depends only on these bytes and the chosen level.
            let slot = (share && level > 0 && (256..=8192).contains(&size))
                .then(|| SHARED_BATCHES.slot(level, &d.scratch));
            if let Some(encoded) = slot.and_then(|s| s.get(level, &d.scratch)) {
                cache.set(Some(d));
                return writer.write_all(&encoded);
            }
            d.output.resize(compress_bound(size), 0);
            let status = d
                .encoder
                .compress(&d.scratch, &mut d.output, DeflateFlush::Finish)
                .map_err(|e| Error::other(format!("Bedrock DEFLATE failed: {e:?}")))?;
            if status != Status::StreamEnd {
                return Err(Error::other("incomplete Bedrock DEFLATE frame"));
            }
            let encoded = &d.output[..d.encoder.total_out() as usize];
            if let Some(s) = slot {
                s.store(level, &d.scratch, encoded);
            }
            let result = writer.write_all(encoded);
            d.encoder.reset();
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
    encoder: Deflate,
    scratch: Vec<u8>,
    output: Vec<u8>,
}

struct SharedBatch {
    level: CompressionLevel,
    plain: Box<[u8]>,
    deflated: Bytes,
}

#[derive(Default)]
struct BatchSlot(Mutex<Option<SharedBatch>>);

impl BatchSlot {
    fn get(&self, level: CompressionLevel, plain: &[u8]) -> Option<Bytes> {
        let guard = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let batch = guard.as_ref()?;
        (batch.level == level && batch.plain.as_ref() == plain).then(|| batch.deflated.clone())
    }

    fn store(&self, level: CompressionLevel, plain: &[u8], encoded: &[u8]) {
        let batch = SharedBatch {
            level,
            plain: plain.into(),
            deflated: Bytes::copy_from_slice(encoded),
        };
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(batch);
    }
}

struct SharedBatches {
    hasher: RandomState,
    // At most 256 world batches of 8 KiB and their compressed bytes.
    slots: [BatchSlot; 256],
}

impl SharedBatches {
    fn slot(&self, level: CompressionLevel, plain: &[u8]) -> &BatchSlot {
        let index = self.hasher.hash_one((level, plain)) as usize % self.slots.len();
        &self.slots[index]
    }
}

static SHARED_BATCHES: LazyLock<SharedBatches> = LazyLock::new(|| SharedBatches {
    hasher: RandomState::new(),
    slots: std::array::from_fn(|_| BatchSlot::default()),
});

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

    #[test]
    fn shared_slot_requires_exact_input_and_level_even_when_reused() {
        let slot = BatchSlot::default();
        slot.store(4, b"first actor", b"first result");
        let in_flight = slot.get(4, b"first actor").unwrap();
        assert!(slot.get(4, b"other actor").is_none());
        assert!(slot.get(5, b"first actor").is_none());
        slot.store(4, b"other actor", b"second result");
        assert_eq!(in_flight.as_ref(), b"first result");
        assert!(slot.get(4, b"first actor").is_none());
        assert_eq!(
            slot.get(4, b"other actor").unwrap().as_ref(),
            b"second result"
        );
    }

    #[test]
    fn reused_terrain_stream_matches_fresh_output_across_negotiated_thresholds() {
        let id = pumpkin_data::bedrock_packet::LEVEL_CHUNK as u16;
        let mut raw = vec![id as u8];
        raw.extend((0..768).map(|i| (i % 113) as u8));
        for level in [1, 4, 9] {
            for prefixed in [false, true] {
                for threshold in [0, 1, 256, 1024] {
                    let mut encoder = BedrockBatchEncoder::new();
                    encoder.set_compression((threshold, level));
                    encoder.set_prefixed_compression(prefixed);
                    let mut fresh = Vec::new();
                    encoder
                        .write_game_packet(
                            id,
                            SubClient::Main,
                            SubClient::Main,
                            &raw[1..],
                            &mut fresh,
                        )
                        .unwrap();
                    for _ in 0..2 {
                        let mut shared = Vec::new();
                        assert!(
                            encoder
                                .try_write_framed_game_packets(
                                    std::iter::once(raw.as_slice()),
                                    &mut shared
                                )
                                .unwrap()
                        );
                        assert_eq!(shared, fresh);
                    }
                }
            }
        }
    }

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
    fn independent_deflate_streams_round_trip_at_the_batch_window_limit() {
        let mut seed = 37u32;
        let noise: Vec<u8> = (0..=16 * 1024)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        let mut enc = BedrockBatchEncoder::new();
        for level in 0..=9 {
            enc.set_compression((1, level));
            for size in [257, 16 * 1024 - 1, 16 * 1024, 16 * 1024 + 1, 1] {
                let mut wire = Vec::new();
                enc.write_batch(size, |out| out.write_all(&noise[..size]), &mut wire)
                    .unwrap();
                assert_eq!(&wire[..2], &[BEDROCK_GAME_PACKET, 0]);
                let mut decoded = Vec::new();
                DeflateDecoder::new(&wire[2..])
                    .read_to_end(&mut decoded)
                    .unwrap();
                assert_eq!(decoded, &noise[..size]);
            }
        }
    }

    #[test]
    fn shared_frames_match_regular_batches_at_compression_boundaries() {
        let first = [0x7f, 0xaa];
        let second = [0x80, 1, 0xbb];
        let mut known = Vec::new();
        let plain = BedrockBatchEncoder::new();
        assert!(
            plain
                .try_write_framed_game_packets([&first[..], &second].into_iter(), &mut known)
                .unwrap()
        );
        assert_eq!(known, [0xfe, 2, 0x7f, 0xaa, 3, 0x80, 1, 0xbb]);

        for payload_len in [0, 125, 126, 127, 128, 17 * 1024] {
            let body: Vec<u8> = (0..payload_len).map(|index| index as u8).collect();
            let mut shared = vec![0xff, 0x7f];
            shared.extend_from_slice(&body);
            let frames = [&first[..], &shared];
            let packets = [(127, &first[1..]), (0x3fff, &body)];
            let size = 3 + shared.len() + VarUInt(shared.len() as u32).written_size();
            for prefixed in [false, true] {
                for threshold in [0, 1, size, size + 1] {
                    let mut encoder = BedrockBatchEncoder::new();
                    encoder.set_prefixed_compression(prefixed);
                    encoder.set_compression((threshold, 4));
                    let mut expected = Vec::new();
                    encoder
                        .write_game_packets(
                            &packets,
                            SubClient::Main,
                            SubClient::Main,
                            &mut expected,
                        )
                        .unwrap();
                    let mut actual = Vec::new();
                    assert!(
                        encoder
                            .try_write_framed_game_packets(frames.into_iter(), &mut actual)
                            .unwrap()
                    );
                    assert_eq!(actual, expected);
                }
            }
        }
    }

    #[test]
    fn shared_frames_request_fallback_before_writing_any_bytes() {
        for invalid_header in [&[][..], &[0x80], &[0x81, 0], &[0x80, 0x80, 1]] {
            let mut output = vec![0xaa];
            let frames = [&[0x13, 0x42][..], invalid_header];
            assert!(
                !BedrockBatchEncoder::new()
                    .try_write_framed_game_packets(frames.into_iter(), &mut output)
                    .unwrap()
            );
            assert_eq!(output, [0xaa]);
        }
    }

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
