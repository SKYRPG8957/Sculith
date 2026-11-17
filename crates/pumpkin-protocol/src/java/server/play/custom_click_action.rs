use crate::{
    ServerPacket,
    ser::{NetworkReadSliceExt, ReadingError},
};
use pumpkin_data::packet::serverbound::play::CUSTOM_CLICK_ACTION;
use pumpkin_macros::java_packet;
use pumpkin_util::version::JavaMinecraftVersion;

#[java_packet(CUSTOM_CLICK_ACTION)]
pub struct SCustomClickAction<'a> {
    pub action_id: &'a str,
    /// Complete unnamed network NBT, including its root tag; None is `TAG_End`.
    pub payload: Option<&'a [u8]>,
}

impl<'a> ServerPacket<'a> for SCustomClickAction<'a> {
    fn read(bytebuf: &mut &'a [u8], _version: &JavaMinecraftVersion) -> Result<Self, ReadingError> {
        Ok(Self {
            action_id: bytebuf.get_str_borrowed()?,
            payload: super::super::custom_click::read_payload(bytebuf)?,
        })
    }
}

impl crate::ClientPacket for SCustomClickAction<'_> {
    fn write_packet_data(
        &self,
        mut write: impl std::io::Write,
        _version: &JavaMinecraftVersion,
    ) -> Result<(), crate::ser::WritingError> {
        use crate::ser::NetworkWriteExt;
        write.write_string(self.action_id)?;
        super::super::custom_click::write_payload(write, self.payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClientPacket;
    use crate::java::server::config::SCustomClickAction as ConfigClick;

    #[test]
    fn custom_click_payloads_match_the_vanilla_codec_in_both_phases() {
        let fixtures: &[(&[u8], &str, Option<usize>)] = &[
            (
                include_bytes!("../../../../tests/fixtures/java_custom_click_none_777.bin"),
                "sculith:form/7/cancel",
                None,
            ),
            (
                include_bytes!("../../../../tests/fixtures/java_custom_click_small_777.bin"),
                "sculith:input/7/submit",
                Some(14),
            ),
            (
                include_bytes!("../../../../tests/fixtures/java_custom_click_large_777.bin"),
                "sculith:input/7/submit",
                Some(173),
            ),
        ];
        // All five official jars use optionalTagCodec().apply(lengthPrefixed(65536)).
        for version in [
            JavaMinecraftVersion::V_1_21_6,
            JavaMinecraftVersion::V_1_21_11,
            JavaMinecraftVersion::V_26_1,
            JavaMinecraftVersion::V_26_2,
            JavaMinecraftVersion::V_26_3,
        ] {
            for &(fixture, action, payload_size) in fixtures {
                let mut play_bytes = fixture;
                let play = SCustomClickAction::read(&mut play_bytes, &version).unwrap();
                let mut config_bytes = fixture;
                let config = ConfigClick::read(&mut config_bytes, &version).unwrap();
                for (id, payload, remaining) in [
                    (play.action_id, play.payload, play_bytes),
                    (config.action_id, config.payload, config_bytes),
                ] {
                    assert_eq!(id, action);
                    assert_eq!(payload.map(<[u8]>::len), payload_size);
                    if let Some([tag, ..]) = payload {
                        assert_eq!(*tag, pumpkin_nbt::COMPOUND_ID);
                    }
                    assert!(remaining.is_empty());
                }
                let mut output = Vec::new();
                play.write_packet_data(&mut output, &version).unwrap();
                assert_eq!(output, fixture);
                output.clear();
                config.write_packet_data(&mut output, &version).unwrap();
                assert_eq!(output, fixture);
            }
        }
    }
}
