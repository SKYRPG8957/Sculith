//! Shared configuration/play framing for vanilla custom-action replies.

use std::io::{Cursor, Write};

use pumpkin_nbt::{
    BYTE_ARRAY_ID, COMPOUND_ID, END_ID, INT_ARRAY_ID, LIST_ID, LONG_ARRAY_ID,
    deserializer::{NbtReadHelper, NbtReadHelperJava},
    tag::NbtTag,
};

use crate::{
    VarInt,
    ser::{NetworkReadExt, NetworkReadSliceExt, NetworkWriteExt, ReadingError, WritingError},
};

// ServerboundCustomClickActionPacket: optionalTagCodec().apply(lengthPrefixed(65536)).
const MAX_PAYLOAD_BYTES: usize = 65_536;
const MAX_TAG_DEPTH: usize = 16;

pub(super) fn read_payload<'a>(read: &mut &'a [u8]) -> Result<Option<&'a [u8]>, ReadingError> {
    let length = usize::try_from(read.get_var_int()?.0)
        .map_err(|_| ReadingError::Message("negative custom-action payload length".into()))?;
    if length == 0 || length > MAX_PAYLOAD_BYTES {
        return Err(ReadingError::TooLarge(
            "custom-action payload length".into(),
        ));
    }
    let payload = read.read_slice_borrowed(length)?;
    if !read.is_empty() {
        return Err(ReadingError::Message("trailing custom-action bytes".into()));
    }
    validate(payload)?;
    Ok((payload != [END_ID]).then_some(payload))
}

pub(super) fn write_payload(
    mut write: impl Write,
    payload: Option<&[u8]>,
) -> Result<(), WritingError> {
    let payload = payload.unwrap_or(&[END_ID]);
    validate(payload).map_err(|error| WritingError::Message(error.to_string()))?;
    write.write_var_int(&VarInt(payload.len() as i32))?;
    write.write_slice(payload)
}

/// Keep opaque network NBT borrowed, while rejecting truncated or deeply nested
/// replies without constructing a tree. Values are decoded by their consumer.
fn validate(payload: &[u8]) -> Result<(), ReadingError> {
    if payload.is_empty() || payload.len() > MAX_PAYLOAD_BYTES {
        return Err(ReadingError::TooLarge(
            "custom-action payload length".into(),
        ));
    }
    let mut cursor = Cursor::new(payload);
    let mut nodes = payload.len();
    let mut reader = NbtReadHelperJava::new(&mut cursor);
    let tag = reader.get_u8().map_err(|error| nbt_error(&error))?;
    skip_tag(&mut reader, tag, 0, &mut nodes).map_err(|error| nbt_error(&error))?;
    if cursor.position() != payload.len() as u64 {
        return Err(ReadingError::Message(
            "invalid custom-action NBT boundary".into(),
        ));
    }
    Ok(())
}

fn nbt_error(error: &pumpkin_nbt::Error) -> ReadingError {
    ReadingError::Message(format!("invalid custom-action NBT: {error}"))
}

fn skip_tag<'a>(
    reader: &mut impl NbtReadHelper<'a>,
    tag: u8,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), pumpkin_nbt::Error> {
    *nodes = nodes
        .checked_sub(1)
        .ok_or(pumpkin_nbt::Error::LargeLength(MAX_PAYLOAD_BYTES))?;
    match tag {
        COMPOUND_ID | LIST_ID if depth >= MAX_TAG_DEPTH => {
            Err(pumpkin_nbt::Error::MaxDepthExceeded)
        }
        COMPOUND_ID => loop {
            let child = reader.get_u8()?;
            if child == END_ID {
                break Ok(());
            }
            reader.skip_string()?;
            skip_tag(reader, child, depth + 1, nodes)?;
        },
        LIST_ID => {
            let child = reader.get_u8()?;
            if child > LONG_ARRAY_ID {
                return Err(pumpkin_nbt::Error::UnknownTagId(child));
            }
            let length = count(reader, 1)?;
            if child == END_ID && length != 0 {
                return Err(pumpkin_nbt::Error::InvalidListTag(child));
            }
            for _ in 0..length {
                skip_tag(reader, child, depth + 1, nodes)?;
            }
            Ok(())
        }
        BYTE_ARRAY_ID | INT_ARRAY_ID | LONG_ARRAY_ID => {
            let width = match tag {
                INT_ARRAY_ID => 4,
                LONG_ARRAY_ID => 8,
                _ => 1,
            };
            let length = count(reader, width)?;
            reader.skip_bytes((length * width) as i64)
        }
        _ => NbtTag::skip_data(reader, tag),
    }
}

fn count<'a>(
    reader: &mut impl NbtReadHelper<'a>,
    width: usize,
) -> Result<usize, pumpkin_nbt::Error> {
    let length = reader.get_i32()?;
    let length = usize::try_from(length).map_err(|_| pumpkin_nbt::Error::NegativeLength(length))?;
    if length > MAX_PAYLOAD_BYTES / width {
        return Err(pumpkin_nbt::Error::LargeLength(length));
    }
    Ok(length)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_framing_truncated_tags_and_unbounded_nesting() {
        for bytes in [
            &[0][..],
            &[0xff, 0xff, 0xff, 0xff, 0x0f],
            &[0x81, 0x80, 4],
            &[1],
            &[1, 0, 0],
            &[1, 13],
            &[1, COMPOUND_ID],
        ] {
            assert!(read_payload(&mut &bytes[..]).is_err(), "{bytes:?}");
        }
        for tag in [
            &[END_ID, 0][..],
            &[8, 0, 5, b'a'],
            &[LIST_ID, 0, 0, 0, 0, 1],
            &[BYTE_ARRAY_ID, 0x7f, 0xff, 0xff, 0xff],
        ] {
            assert!(validate(tag).is_err(), "{tag:?}");
        }
        for depth in [16, 17] {
            let mut tag = vec![COMPOUND_ID];
            for _ in 1..depth {
                tag.extend_from_slice(&[COMPOUND_ID, 0, 0]);
            }
            tag.extend(std::iter::repeat_n(END_ID, depth));
            assert_eq!(validate(&tag).is_ok(), depth == 16);
        }
    }
}
