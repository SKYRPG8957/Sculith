//! Protocol-777 public chat. Offline sessions broadcast unsigned system text;
//! consuming the signature envelope does not authenticate it.
use super::codec::Fields;
use anyhow::{Result, ensure};

pub fn read(mut fields: Fields<'_>) -> Result<String> {
    let message = fields.string(256)?.to_owned();
    fields.long()?; // Sender timestamp, never used as an owner clock.
    fields.long()?; // Salt.
    if fields.boolean()? {
        fields.take(256)?;
    }
    ensure!(fields.int()? >= 0, "negative chat acknowledgement offset");
    let acknowledged = fields.take(3)?;
    ensure!(
        acknowledged[2] & 0xf0 == 0,
        "chat acknowledgement exceeds 20 bits"
    );
    fields.byte()?; // Last-seen checksum; no signed messages are issued here.
    fields.end()?;
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::java::write_string;

    fn envelope(signed: bool) -> Vec<u8> {
        let mut bytes = vec![];
        write_string("안녕 😀", &mut bytes);
        bytes.extend_from_slice(&[0; 16]);
        bytes.push(u8::from(signed));
        if signed {
            bytes.extend_from_slice(&[1; 256]);
        }
        bytes.extend_from_slice(&[0, 0, 0, 0, 0]);
        bytes
    }

    #[test]
    fn consumes_signed_and_unsigned_envelopes_without_trusting_signatures() {
        for signed in [false, true] {
            let bytes = envelope(signed);
            assert_eq!(read(Fields(&bytes)).unwrap(), "안녕 😀");
            for length in 0..bytes.len() {
                assert!(read(Fields(&bytes[..length])).is_err());
            }
            let mut trailing = bytes;
            trailing.push(0);
            assert!(read(Fields(&trailing)).is_err());
        }
    }

    #[test]
    fn refuses_invalid_acknowledgement_bits() {
        let mut bytes = envelope(false);
        let last = bytes.len() - 2;
        bytes[last] = 0x80;
        assert!(read(Fields(&bytes)).is_err());
    }
}
