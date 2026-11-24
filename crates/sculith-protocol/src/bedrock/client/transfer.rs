use pumpkin_macros::packet;

use std::io::{Error, ErrorKind, Read, Write};

use crate::serial::{PacketRead, PacketWrite};

/// Instructs the client to disconnect and reconnect to another server address and port.
#[packet(pumpkin_data::bedrock_packet::TRANSFER)]
pub struct CTransfer {
    /// Destination server hostname or IP address.
    pub server_address: String,
    /// Destination server UDP port.
    pub server_port: u16,
    /// Whether the client should reload its local world context upon transferring.
    pub reload_world: bool,
}

impl PacketWrite for CTransfer {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.server_address.write(writer)?;
        self.server_port.write(writer)?;
        self.reload_world.write(writer)?;
        // Optional GatheringsConfigurationJoinInfo.
        false.write(writer)
    }
}

impl PacketRead for CTransfer {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let server_address = String::read(reader)?;
        let server_port = u16::read(reader)?;
        let reload_world = bool::read(reader)?;
        if bool::read(reader)? {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "Bedrock transfer includes unsupported gathering join information",
            ));
        }
        Ok(Self::new(server_address, server_port, reload_world))
    }
}

impl CTransfer {
    /// Constructs a transfer packet with target server connection parameters.
    #[must_use]
    pub const fn new(server_address: String, server_port: u16, reload_world: bool) -> Self {
        Self {
            server_address,
            server_port,
            reload_world,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_direct_transfer() {
        let mut body = Vec::new();
        CTransfer::new("localhost".into(), 19132, false)
            .write(&mut body)
            .unwrap();
        let mut body = body.as_slice();
        let packet = CTransfer::read(&mut body).unwrap();
        assert!(body.is_empty());
        assert_eq!(packet.server_address, "localhost");
        assert_eq!(packet.server_port, 19132);
        assert!(!packet.reload_world);
    }
}
