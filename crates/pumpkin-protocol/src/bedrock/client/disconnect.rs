// Last verified for v2169

use pumpkin_macros::packet;
use std::io::{Error, ErrorKind, Read, Write};

use crate::{
    codec::{var_int::VarInt, var_uint::VarUInt},
    serial::{PacketRead, PacketWrite},
};

/// Sent by the server to cleanly disconnect a client with an optional error code and kick message.
#[packet(pumpkin_data::bedrock_packet::DISCONNECT)]
pub struct CDisconnect {
    /// Disconnect reason code determining the message shown on the client disconnect screen.
    pub reason: VarInt,
    /// Whether to skip showing the disconnect message dialog and return directly to the main menu.
    pub skip_message: bool,
    /// Disconnect explanation message displayed to the user.
    pub message: String,
    /// Profanity-filtered variant of the disconnect message.
    pub filtered_message: String,
}

impl CDisconnect {
    #[must_use]
    pub const fn new(reason: i32, message: String) -> Self {
        Self {
            reason: VarInt(reason),
            skip_message: message.is_empty(),
            message,
            filtered_message: String::new(),
        }
    }
}

impl PacketWrite for CDisconnect {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.reason.write(writer)?;
        VarUInt(u32::from(self.skip_message)).write(writer)?;
        if !self.skip_message {
            self.message.write(writer)?;
            self.filtered_message.write(writer)?;
        }
        Ok(())
    }
}

impl PacketRead for CDisconnect {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let reason = VarInt::read(reader)?;
        match VarUInt::read(reader)?.0 {
            0 => Ok(Self {
                reason,
                skip_message: false,
                message: String::read(reader)?,
                filtered_message: String::read(reader)?,
            }),
            1 => Ok(Self {
                reason,
                skip_message: true,
                message: String::new(),
                filtered_message: String::new(),
            }),
            _ => Err(Error::new(
                ErrorKind::InvalidData,
                "invalid Bedrock disconnect message selector",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_both_disconnect_message_variants() {
        for message in ["", "Disconnected"] {
            let mut body = Vec::new();
            CDisconnect::new(8, message.into())
                .write(&mut body)
                .unwrap();
            let mut body = body.as_slice();
            let packet = CDisconnect::read(&mut body).unwrap();
            assert!(body.is_empty());
            assert_eq!(packet.reason.0, 8);
            assert_eq!(packet.skip_message, message.is_empty());
            assert_eq!(packet.message, message);
        }
    }
}
