use crate::{
    codec::var_ulong::VarULong,
    serial::{PacketRead, PacketWrite},
};
use pumpkin_macros::packet;
use pumpkin_util::math::vector3::Vector3;
use std::io::{Error, Read, Write};

/// Sent by the server to update a player's movement, teleport, or view angle.
#[derive(Debug)]
#[packet(pumpkin_data::bedrock_packet::MOVE_PLAYER)]
pub struct CMovePlayer {
    /// Runtime entity ID of the player being moved.
    pub player_runtime_id: VarULong,
    /// Absolute position coordinates in world space.
    pub position: Vector3<f32>,
    /// Pitch angle in degrees.
    pub pitch: f32,
    /// Yaw angle in degrees.
    pub yaw: f32,
    /// Horizontal head yaw rotation angle in degrees.
    pub head_yaw: f32,
    /// Movement mode (normal, reset, teleport, or rotation).
    pub mode: u8,
    /// Whether the player is resting on the ground.
    pub on_ground: bool,
    /// Runtime entity ID of the vehicle the player is riding, or zero.
    pub riding_runtime_id: VarULong,
    /// Cause of the teleportation (present when mode is `MODE_TELEPORT`).
    pub teleport_cause: i32,
    /// Entity type that triggered the teleportation.
    pub teleport_source_entity_type: i32,
    /// World simulation tick number.
    pub tick: VarULong,
}

impl CMovePlayer {
    /// Standard movement interpolation mode.
    pub const MODE_NORMAL: u8 = 0;
    /// Reset interpolation mode.
    pub const MODE_RESET: u8 = 1;
    /// Instantaneous teleportation mode.
    pub const MODE_TELEPORT: u8 = 2;
    /// Rotation-only update mode without changing position.
    pub const MODE_ROTATION: u8 = 3;

    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub const fn new(
        player_runtime_id: VarULong,
        position: Vector3<f32>,
        pitch: f32,
        yaw: f32,
        head_yaw: f32,
        mode: u8,
        on_ground: bool,
        riding_runtime_id: VarULong,
        teleport_cause: i32,
        teleport_source_entity_type: i32,
        tick: VarULong,
    ) -> Self {
        Self {
            player_runtime_id,
            position,
            pitch,
            yaw,
            head_yaw,
            mode,
            on_ground,
            riding_runtime_id,
            teleport_cause,
            teleport_source_entity_type,
            tick,
        }
    }
}

impl PacketWrite for CMovePlayer {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.player_runtime_id.write(writer)?;
        self.position.write(writer)?;
        self.pitch.write(writer)?;
        self.yaw.write(writer)?;
        self.head_yaw.write(writer)?;
        self.mode.write(writer)?;
        self.on_ground.write(writer)?;
        self.riding_runtime_id.write(writer)?;
        (self.mode == Self::MODE_TELEPORT).write(writer)?;
        if self.mode == Self::MODE_TELEPORT {
            self.teleport_cause.write(writer)?;
            self.teleport_source_entity_type.write(writer)?;
        }
        self.tick.write(writer)?;
        Ok(())
    }
}

impl PacketRead for CMovePlayer {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let player_runtime_id = VarULong::read(reader)?;
        let position = Vector3::read(reader)?;
        let pitch = f32::read(reader)?;
        let yaw = f32::read(reader)?;
        let head_yaw = f32::read(reader)?;
        let mode = u8::read(reader)?;
        let on_ground = bool::read(reader)?;
        let riding_runtime_id = VarULong::read(reader)?;
        let (teleport_cause, teleport_source_entity_type) = if bool::read(reader)? {
            (i32::read(reader)?, i32::read(reader)?)
        } else {
            (0, 0)
        };
        Ok(Self {
            player_runtime_id,
            position,
            pitch,
            yaw,
            head_yaw,
            mode,
            on_ground,
            riding_runtime_id,
            teleport_cause,
            teleport_source_entity_type,
            tick: VarULong::read(reader)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_teleport_data_before_the_world_tick() {
        let mut body: &[u8] = &[
            2, // actor runtime ID
            0, 0, 0x80, 0x3f, 0, 0, 0, 0x40, 0, 0, 0x40, 0x40, // position
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // angles
            2, 1, 0, 1, // teleport mode, grounded, ridden entity, data present
            3, 0, 0, 0, 20, 0, 0, 0, // cause and entity type
            0xac, 2,
        ];
        let packet = CMovePlayer::read(&mut body).unwrap();
        assert_eq!(packet.position, Vector3::new(1.0, 2.0, 3.0));
        assert_eq!(
            (packet.teleport_cause, packet.teleport_source_entity_type),
            (3, 20)
        );
        assert_eq!(packet.tick.0, 300);
        assert!(body.is_empty());
    }
}
