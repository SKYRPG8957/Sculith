use pumpkin_macros::packet;

use crate::serial::PacketWrite;

/// Stop a named sound or all sounds. Bedrock has no per-category selector.
#[derive(PacketWrite)]
#[packet(pumpkin_data::bedrock_packet::STOP_SOUND)]
pub struct CStopSound {
    pub sound_name: String,
    pub stop_all: bool,
    pub stop_music_legacy: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_stop_matches_independent_2225_bytes() {
        let mut bytes = Vec::new();
        CStopSound {
            sound_name: "sculith.demo_tone".into(),
            stop_all: false,
            stop_music_legacy: false,
        }
        .write(&mut bytes)
        .unwrap();
        assert_eq!(
            bytes,
            include_bytes!("../../../tests/fixtures/stop_sound_2225.bin")
        );
    }
}
