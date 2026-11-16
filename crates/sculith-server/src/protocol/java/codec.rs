//! Bounded Java framing. The reader retains partial progress across select! cancellation.
use anyhow::{Context, Result, ensure};
use sculith_world::BlockPos;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::{read_varint, write_string, write_varint};
pub const MAX_INBOUND: usize = 8192;
const MAX_OUTBOUND: usize = 1 << 20;

#[derive(Default)]
pub struct Frames {
    prefix: Vec<u8>,
    expected: usize,
    body: Vec<u8>,
    offset: usize,
    trace: crate::connections::Trace,
    phase: Option<sculith_core::connections::PacketPhase>,
    protocol: Option<u32>,
}

impl Frames {
    pub fn observed(observer: &crate::connections::Observer) -> Self {
        Self {
            trace: observer.open(sculith_core::connections::ConnectionTransport::JavaTcp),
            ..Self::default()
        }
    }

    pub fn select_protocol(&mut self, protocol: u32) {
        self.protocol = Some(protocol);
    }

    pub fn set_phase(&mut self, phase: sculith_core::connections::PacketPhase) {
        self.phase = Some(phase);
    }

    pub fn attach(&mut self, player_id: u64) {
        self.trace.attach(player_id);
    }

    pub async fn next(&mut self, stream: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>> {
        if self.expected == 0 {
            while self.prefix.last().is_none_or(|b| b & 128 != 0) {
                ensure!(self.prefix.len() < 5, "frame length exceeds five bytes");
                let mut byte = [0];
                ensure!(stream.read(&mut byte).await? == 1, "Java peer closed");
                self.prefix.push(byte[0]);
            }
            let length = read_varint(&mut &self.prefix[..])?;
            ensure!(
                (1..=MAX_INBOUND as i32).contains(&length),
                "frame length out of bounds"
            );
            self.expected = length as usize;
            self.body.resize(self.expected, 0);
        }
        while self.offset < self.expected {
            let count = stream.read(&mut self.body[self.offset..]).await?;
            ensure!(count != 0, "Java peer closed in a frame");
            self.offset += count;
        }
        self.prefix.clear();
        self.offset = 0;
        self.expected = 0;
        let body = std::mem::take(&mut self.body);
        if self.trace.enabled() {
            // A malformed identifier has no decoded packet metadata. The
            // lifecycle still closes, without forwarding any client bytes.
            if let Ok(id) = read_varint(&mut &body[..])
                && let Ok(packet) = crate::protocol::catalog::java_connection_receipt(
                    id,
                    self.protocol,
                    self.phase
                        .unwrap_or(sculith_core::connections::PacketPhase::Handshake),
                )
            {
                self.trace.packet(packet);
            }
        }
        Ok(body)
    }
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    use sculith_core::connections::{ConnectionEvent, PacketPhase};

    #[tokio::test]
    async fn partial_cancelled_frame_emits_once_after_completion_without_body() {
        let observer = crate::connections::Observer::default();
        let mut events = observer.subscribe();
        observer.configure(true);
        let mut frames = Frames::observed(&observer);
        frames.select_protocol(777);
        frames.set_phase(PacketPhase::Login);
        let (mut input, mut output) = tokio::io::duplex(64);
        input.write_all(&[4, 0, b'x']).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(5), frames.next(&mut output))
                .await
                .is_err()
        );
        assert!(matches!(
            events.try_recv().unwrap(),
            ConnectionEvent::Opened { .. }
        ));
        assert!(events.try_recv().is_err());
        input.write_all(b"yz").await.unwrap();
        assert_eq!(frames.next(&mut output).await.unwrap(), b"\0xyz");
        let ConnectionEvent::PacketReceived { packet, .. } = events.try_recv().unwrap() else {
            panic!("missing receipt");
        };
        assert_eq!(
            (packet.sequence, packet.phase, packet.name.as_str()),
            (1, PacketPhase::Login, "minecraft:hello")
        );
        let fields = serde_json::to_value(packet).unwrap();
        assert_eq!(fields.as_object().unwrap().len(), 6);
        assert!(!fields.to_string().contains("xyz"));
        drop(frames);
        assert!(matches!(
            events.try_recv().unwrap(),
            ConnectionEvent::Closed { .. }
        ));
        assert!(events.try_recv().is_err());
    }
}

pub async fn send(stream: &mut (impl AsyncWrite + Unpin), payload: &[u8]) -> Result<()> {
    ensure!(
        !payload.is_empty() && payload.len() <= MAX_OUTBOUND,
        "outbound frame out of bounds"
    );
    let mut frame = Vec::with_capacity(payload.len() + 5);
    write_varint(payload.len() as i32, &mut frame);
    frame.extend_from_slice(payload);
    tokio::time::timeout(Duration::from_secs(5), stream.write_all(&frame))
        .await
        .context("Java writer stalled")??;
    Ok(())
}

pub fn packet(id: i32) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_varint(id, &mut bytes);
    bytes
}

pub struct Fields<'a>(pub &'a [u8]);
impl<'a> Fields<'a> {
    pub fn int(&mut self) -> Result<i32> {
        read_varint(&mut self.0)
    }
    pub fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        ensure!(count <= self.0.len(), "truncated Java field");
        let (head, tail) = self.0.split_at(count);
        self.0 = tail;
        Ok(head)
    }
    pub fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn boolean(&mut self) -> Result<bool> {
        let value = self.byte()?;
        ensure!(value <= 1, "invalid Java boolean");
        Ok(value != 0)
    }
    pub fn short(&mut self) -> Result<i16> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into()?))
    }
    pub fn long(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into()?))
    }
    pub fn double(&mut self) -> Result<f64> {
        Ok(f64::from_be_bytes(self.take(8)?.try_into()?))
    }
    pub fn float(&mut self) -> Result<f32> {
        Ok(f32::from_be_bytes(self.take(4)?.try_into()?))
    }
    pub fn string(&mut self, max_chars: usize) -> Result<&'a str> {
        let length = self.int()?;
        ensure!(
            length >= 0 && length as usize <= max_chars * 4,
            "Java string length out of bounds"
        );
        let value = std::str::from_utf8(self.take(length as usize)?)?;
        ensure!(
            value.chars().count() <= max_chars,
            "too many Java string characters"
        );
        Ok(value)
    }
    pub fn end(self) -> Result<()> {
        ensure!(
            self.0.is_empty(),
            "trailing Java fields ({} bytes)",
            self.0.len()
        );
        Ok(())
    }
    pub fn position(&mut self) -> Result<BlockPos> {
        let word = self.long()?;
        Ok(BlockPos {
            x: (word >> 38) as i32,
            y: (word << 52 >> 52) as i32,
            z: (word << 26 >> 38) as i32,
        })
    }
}

pub fn position(value: BlockPos, bytes: &mut Vec<u8>) {
    let packed = ((i64::from(value.x) & 0x3ffffff) << 38)
        | ((i64::from(value.z) & 0x3ffffff) << 12)
        | (i64::from(value.y) & 0xfff);
    bytes.extend_from_slice(&packed.to_be_bytes());
}

pub fn text(value: &str, bytes: &mut Vec<u8>) -> Result<()> {
    ensure!(value.len() <= 4096, "Java text component too large");
    // Rust String goes through Java modified UTF-8. Value::String holds raw
    // BString bytes and intentionally bypasses this conversion (emoji failed
    // in the actual 26.3 client when passed through that raw representation).
    append_nbt(&nbtx::to_be_bytes(&value.to_owned())?, bytes)
}

pub fn nbt(value: &nbtx::Value, bytes: &mut Vec<u8>) -> Result<()> {
    append_nbt(&nbtx::to_be_bytes(value)?, bytes)
}

pub fn nbt_string(value: &str) -> Result<nbtx::Value> {
    let encoded = nbtx::to_be_bytes(&value.to_owned())?;
    ensure!(
        encoded.first() == Some(&8) && encoded.get(1..3) == Some(&[0, 0]) && encoded.len() >= 5,
        "unexpected NBT string layout"
    );
    // Value's BString is raw bytes, so populate it with Java MUTF-8 from the
    // library's Rust String encoder, including supplementary surrogate pairs.
    Ok(nbtx::Value::String(encoded[5..].to_vec().into()))
}

fn append_nbt(encoded: &[u8], bytes: &mut Vec<u8>) -> Result<()> {
    ensure!(encoded.len() <= 16 * 1024, "Java NBT component too large");
    // Network NBT omits the root name present in standard disk NBT.
    ensure!(
        encoded.get(1..3) == Some(&[0, 0]),
        "unexpected NBT root name"
    );
    bytes.push(encoded[0]);
    bytes.extend_from_slice(&encoded[3..]);
    Ok(())
}

pub fn strings(values: &[&str], bytes: &mut Vec<u8>) {
    write_varint(values.len() as i32, bytes);
    for value in values {
        write_string(value, bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn literal_text_encodes_supplementary_unicode_as_java_surrogate_pairs() {
        let mut bytes = Vec::new();
        text("👋", &mut bytes).unwrap();
        assert_eq!(bytes, [8, 0, 6, 0xed, 0xa0, 0xbd, 0xed, 0xb1, 0x8b]);
    }
    #[tokio::test]
    async fn partial_frame_survives_cancellation_between_world_events() {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        let mut frames = Frames::default();
        writer.write_all(&[0x82]).await.unwrap(); // Incomplete 130-byte frame prefix.
        assert!(
            tokio::time::timeout(Duration::from_millis(10), frames.next(&mut reader))
                .await
                .is_err()
        );
        writer.write_all(&[1, 7, 8, 9]).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), frames.next(&mut reader))
                .await
                .is_err()
        );
        writer.write_all(&[6; 127]).await.unwrap();
        let payload = frames.next(&mut reader).await.unwrap();
        assert_eq!(payload.len(), 130);
        assert_eq!(&payload[..3], &[7, 8, 9]);
    }

    #[test]
    fn signed_block_positions_include_negative_chunks_and_sections() {
        for value in [
            BlockPos {
                x: -17,
                y: -64,
                z: -1,
            },
            BlockPos {
                x: 30_000_000,
                y: 319,
                z: -30_000_000,
            },
        ] {
            let mut bytes = vec![];
            position(value, &mut bytes);
            assert_eq!(Fields(&bytes).position().unwrap(), value);
        }
    }
}
