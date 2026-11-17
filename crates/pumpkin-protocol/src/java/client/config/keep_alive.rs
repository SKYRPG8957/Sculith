use pumpkin_data::packet::clientbound::config::KEEP_ALIVE;
use pumpkin_macros::java_packet;
use pumpkin_util::version::JavaMinecraftVersion;

use crate::{
    ClientPacket,
    ser::{NetworkWriteExt, WritingError},
};

#[java_packet(KEEP_ALIVE)]
pub struct CConfigKeepAlive {
    pub id: i64,
}

impl CConfigKeepAlive {
    #[must_use]
    pub const fn new(id: i64) -> Self {
        Self { id }
    }
}

impl ClientPacket for CConfigKeepAlive {
    fn write_packet_data(
        &self,
        mut write: impl std::io::Write,
        _version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        write.write_i64_be(self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::MultiVersionJavaPacket;

    #[test]
    fn config_keepalive_has_the_current_id_and_signed_big_endian_payload() {
        let mut bytes = Vec::new();
        let id = -0x1020_3040_5060_7080i64;
        CConfigKeepAlive::new(id)
            .write_packet_data(&mut bytes, &pumpkin_data::packet::CURRENT_MC_VERSION)
            .unwrap();
        assert_eq!(bytes, id.to_be_bytes());
        assert_eq!(
            CConfigKeepAlive::to_id(pumpkin_data::packet::CURRENT_MC_VERSION),
            4
        );
    }
}
