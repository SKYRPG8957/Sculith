use std::io::{Error, ErrorKind, Read, Write};

use pumpkin_macros::packet;

use crate::{
    codec::var_uint::VarUInt,
    serial::{PacketRead, PacketWrite},
};

/// Sent by the server to update gamerules on the client (e.g. `showcoordinates` or `dodaylightcycle`).
#[derive(PacketRead, PacketWrite)]
#[packet(pumpkin_data::bedrock_packet::GAME_RULES_CHANGED)]
pub struct CGamerulesChanged {
    /// List of game rules being updated.
    pub rule_data: Vec<GameRule>,
}

/// An individual gamerule entry with its identifier, edit permission, and typed value.
#[derive(PacketRead, PacketWrite)]
pub struct GameRule {
    /// Name of the gamerule in lowercase (e.g. `showcoordinates`).
    pub rule_name: String,
    /// Whether players with operator permissions can edit this gamerule in-game.
    pub rule_can_be_modified: bool,
    /// Value associated with the gamerule.
    pub rule_value: RuleValue,
}

pub enum RuleValue {
    Null,
    Boolean(bool),
    Integer(u32),
    Float(f32),
}

impl PacketWrite for RuleValue {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        match self {
            Self::Null => VarUInt(0).write(writer),
            Self::Boolean(value) => {
                VarUInt(1).write(writer)?;
                value.write(writer)
            }
            Self::Integer(value) => {
                VarUInt(2).write(writer)?;
                value.write(writer)
            }
            Self::Float(value) => {
                VarUInt(3).write(writer)?;
                value.write(writer)
            }
        }
    }
}

impl PacketRead for RuleValue {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        match VarUInt::read(reader)?.0 {
            0 => Ok(Self::Null),
            1 => Ok(Self::Boolean(bool::read(reader)?)),
            2 => Ok(Self::Integer(u32::read(reader)?)),
            3 => Ok(Self::Float(f32::read(reader)?)),
            _ => Err(Error::new(
                ErrorKind::InvalidData,
                "invalid Bedrock game rule type",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_fixed_width_integer_rule_values() {
        let mut bytes = Vec::new();
        RuleValue::Integer(256).write(&mut bytes).unwrap();
        assert_eq!(bytes, [2, 0, 1, 0, 0]);
        let mut body = bytes.as_slice();
        assert!(matches!(
            RuleValue::read(&mut body).unwrap(),
            RuleValue::Integer(256)
        ));
        assert!(body.is_empty());
    }
}
