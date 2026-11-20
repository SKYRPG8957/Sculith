use std::io::{Error, ErrorKind, Read};

use pumpkin_macros::packet;

use crate::{codec::var_long::VarLong, serial::PacketRead};

const ACTOR_FLAG_COUNT: usize = 131;
const ACTOR_FLAG_WORDS: usize = ACTOR_FLAG_COUNT.div_ceil(64);

/// The client's actor flags and movement prediction state after an authoritative correction.
#[packet(pumpkin_data::bedrock_packet::CLIENT_MOVEMENT_PREDICTION_SYNC)]
pub struct SClientMovementPredictionSync {
    /// Set flags as 64-bit words, starting with the lowest numbered flags.
    pub actor_flags: [u64; ACTOR_FLAG_WORDS],
    pub bounding_box_scale: f32,
    pub bounding_box_width: f32,
    pub bounding_box_height: f32,
    pub movement_speed: f32,
    pub underwater_movement_speed: f32,
    pub lava_movement_speed: f32,
    pub jump_strength: f32,
    pub health: f32,
    pub hunger: f32,
    pub friction_modifier: f32,
    pub bounciness: f32,
    pub air_drag_modifier: f32,
    pub actor_unique_id: VarLong,
    pub flying: bool,
}

fn read_actor_flags(reader: &mut impl Read) -> Result<[u64; ACTOR_FLAG_WORDS], Error> {
    let mut words = [0; ACTOR_FLAG_WORDS];
    for index in (0..ACTOR_FLAG_COUNT).step_by(7) {
        let byte = u8::read(reader)?;
        let flags = u64::from(byte & 0x7f);
        let available_bits = (ACTOR_FLAG_COUNT - index).min(7);
        if flags >= 1 << available_bits {
            break;
        }
        let word = index / 64;
        let bit = index % 64;
        words[word] |= flags << bit;
        if bit > 57 {
            words[word + 1] |= flags >> (64 - bit);
        }
        if byte & 0x80 == 0 {
            return Ok(words);
        }
    }
    Err(Error::new(
        ErrorKind::InvalidData,
        "Bedrock actor flag bitset is too large",
    ))
}

impl PacketRead for SClientMovementPredictionSync {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        Ok(Self {
            actor_flags: read_actor_flags(reader)?,
            bounding_box_scale: f32::read(reader)?,
            bounding_box_width: f32::read(reader)?,
            bounding_box_height: f32::read(reader)?,
            movement_speed: f32::read(reader)?,
            underwater_movement_speed: f32::read(reader)?,
            lava_movement_speed: f32::read(reader)?,
            jump_strength: f32::read(reader)?,
            health: f32::read(reader)?,
            hunger: f32::read(reader)?,
            friction_modifier: f32::read(reader)?,
            bounciness: f32::read(reader)?,
            air_drag_modifier: f32::read(reader)?,
            actor_unique_id: VarLong::read(reader)?,
            flying: bool::read(reader)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial::PacketWrite;

    #[test]
    fn preserves_flags_above_the_second_word() {
        let mut encoded = vec![0x80; 18];
        encoded[0] = 0xc0;
        encoded[9] = 0x82;
        encoded.push(0x10);
        [1.0f32; 12].write(&mut encoded).unwrap();
        VarLong(-42).write(&mut encoded).unwrap();
        true.write(&mut encoded).unwrap();

        let mut body = encoded.as_slice();
        let packet = SClientMovementPredictionSync::read(&mut body).unwrap();
        assert!(body.is_empty());
        assert_eq!(packet.actor_flags, [64, 1, 4]);
        assert_eq!(packet.actor_unique_id, VarLong(-42));
        assert!(packet.flying);
    }
}
