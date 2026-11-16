//! Reusable JSON-lines storage. Length checks happen before growth or pipe writes.
use std::io::{self, Write};

use anyhow::Result;
use sculith_plugin_api::{Event, MAX_MESSAGE_BYTES};
use serde::Serialize;

#[derive(Default)]
pub(super) struct Buffer(Vec<u8>);

impl Buffer {
    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }

    #[inline]
    pub fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > MAX_MESSAGE_BYTES - self.0.len() {
            return Err(io::Error::other("plugin message exceeds 64 KiB"));
        }
        let required = self.0.len() + bytes.len();
        if required > self.0.capacity() {
            self.grow(required);
        }
        self.0.extend_from_slice(bytes);
        Ok(())
    }

    #[cold]
    fn grow(&mut self, required: usize) {
        let target = required
            .max(self.0.capacity().saturating_mul(2))
            .min(MAX_MESSAGE_BYTES);
        self.0.reserve_exact(target - self.0.len());
    }
}

impl Write for Buffer {
    #[inline]
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.append(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Serialize)]
struct WireRequest<'a, T> {
    id: u64,
    message: &'a T,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum EventMessage<'a> {
    Event { event: &'a Event },
}

pub(super) fn request<T: Serialize>(buffer: &mut Buffer, id: u64, message: &T) -> Result<()> {
    buffer.clear();
    serde_json::to_writer(&mut *buffer, &WireRequest { id, message })?;
    buffer.append(b"\n")?;
    Ok(())
}

pub(super) fn event(buffer: &mut Buffer, event: &Event) -> Result<()> {
    buffer.clear();
    serde_json::to_writer(buffer, &EventMessage::Event { event })?;
    Ok(())
}

/// Only accepts a body produced by `event`, never arbitrary plugin JSON.
pub(super) fn shared_request(buffer: &mut Buffer, id: u64, body: &[u8]) -> Result<()> {
    buffer.clear();
    write!(buffer, "{{\"id\":{id},\"message\":")?;
    buffer.append(body)?;
    buffer.append(b"}\n")?;
    Ok(())
}
