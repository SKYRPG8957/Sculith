use pumpkin_macros::packet;
use sculith_util::math::{position::BlockPos, vector3::Vector3};

use crate::{codec::var_int::VarInt, serial::PacketWrite};

/// Play a named sound. Position components are signed eighth-block coordinates.
/// Field layout verified against gophertunnel for Bedrock protocol 2225.
#[derive(PacketWrite)]
#[packet(pumpkin_data::bedrock_packet::PLAY_SOUND)]
pub struct CPlaySound {
    pub sound_name: String,
    pub position: BlockPos,
    pub volume: f32,
    pub pitch: f32,
    pub loop_count: VarInt,
    pub bypass_listener_range_check: bool,
    pub handle: Option<u64>,
    pub playback_position_seconds: Option<f32>,
}

impl CPlaySound {
    /// Construct a non-looping, spatial sound without saturating invalid coordinates.
    #[must_use]
    pub fn new(
        sound_name: String,
        position: &Vector3<f64>,
        volume: f32,
        pitch: f32,
    ) -> Option<Self> {
        if !volume.is_finite() || !pitch.is_finite() || volume < 0.0 || pitch < 0.0 {
            return None;
        }
        let mut encoded = [0; 3];
        for (target, component) in encoded.iter_mut().zip([position.x, position.y, position.z]) {
            // Bedrock quantizes single-precision coordinates before scaling.
            let value = f64::from((component as f32 * 8.0).trunc());
            if !(f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&value) {
                return None;
            }
            *target = value as i32;
        }
        Some(Self {
            sound_name,
            position: BlockPos::new(encoded[0], encoded[1], encoded[2]),
            volume,
            pitch,
            loop_count: VarInt(-1),
            bypass_listener_range_check: false,
            handle: None,
            playback_position_seconds: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sound_position_and_optional_fields_match_independent_2225_bytes() {
        let mut packet = CPlaySound::new(
            "sculith.demo_tone".into(),
            &Vector3::new(-1.25, -60.5, 3.875),
            2.5,
            0.75,
        )
        .unwrap();
        packet.handle = Some(42);
        packet.playback_position_seconds = Some(1.25);
        let mut bytes = Vec::new();
        packet.write(&mut bytes).unwrap();
        assert_eq!(
            bytes,
            include_bytes!("../../../tests/fixtures/play_sound_2225.bin")
        );
        assert!(
            CPlaySound::new("test".into(), &Vector3::new(f64::NAN, 0.0, 0.0), 1.0, 1.0).is_none()
        );
        assert!(
            CPlaySound::new(
                "test".into(),
                &Vector3::new(0.0, 0.0, 0.0),
                f32::INFINITY,
                1.0
            )
            .is_none()
        );
    }
}
