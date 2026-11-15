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
}

impl Frames {
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
        Ok(std::mem::take(&mut self.body))
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
        ensure!(self.0.is_empty(), "trailing Java fields");
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
    let encoded = nbtx::to_be_bytes(&nbtx::Value::String(value.into()))?;
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
