use std::io::{Error, Write};

use uuid::Uuid;

use crate::serial::PacketWrite;

impl PacketWrite for Uuid {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        let (high, low) = self.as_u64_pair();
        writer.write_all(&high.to_le_bytes())?;
        writer.write_all(&low.to_le_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ser::{NetworkReadExt, NetworkWriteExt};
    use crate::serial::{PacketRead, PacketReadSlice};

    #[test]
    fn bedrock_uuid_matches_two_little_endian_words_and_both_readers() {
        // gophertunnel e2251a60f71d UUID encoding, including the half-word order.
        let uuid = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let wire = [
            0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99,
            0x88,
        ];
        let mut output = Vec::new();
        uuid.write(&mut output).unwrap();
        assert_eq!(output, wire);
        assert_eq!(Uuid::read(&mut wire.as_slice()).unwrap(), uuid);
        assert_eq!(Uuid::read_slice(&mut wire.as_slice()).unwrap(), uuid);

        let mut java = Vec::new();
        java.write_uuid(&uuid).unwrap();
        assert_eq!(java, uuid.as_bytes());
        assert_eq!(java.as_slice().get_uuid().unwrap(), uuid);
    }
}
