use std::io::{Error, ErrorKind, Read, Write};

use crate::{
    codec::{var_int::VarInt, var_long::VarLong, var_uint::VarUInt},
    serial::{PacketRead, PacketWrite},
};
use pumpkin_macros::packet;

const SCORE_ENTRY_VARIANTS: [&str; 4] =
    ["remove", "changeplayer", "changeentity", "changefakeplayer"];

/// Updates or removes scoreboard entries displayed to the client.
#[packet(pumpkin_data::bedrock_packet::SET_SCORE)]
pub struct CSetScore {
    /// Action type determining whether scores are modified (0) or removed (1).
    pub action: VarInt, // 0 = change, 1 = remove
    /// Score entries affected by this update.
    pub entries: Vec<ScoreEntry>,
}

impl PacketWrite for CSetScore {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        VarUInt(self.entries.len() as u32).write(writer)?;
        for entry in &self.entries {
            let entry_type = if self.action.0 == 1 {
                0
            } else {
                entry.entry_type.0
            };
            let name = SCORE_ENTRY_VARIANTS
                .get(entry_type as usize)
                .ok_or_else(|| Error::other("Invalid scoreboard entry type"))?;
            VarUInt(entry_type as u32).write(writer)?;
            name.write(writer)?;
            entry.write_for_type(writer, entry_type)?;
        }
        Ok(())
    }
}

impl PacketRead for CSetScore {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let entries = Vec::<ScoreEntry>::read(reader)?;
        let removing_entire_list =
            !entries.is_empty() && entries.iter().all(|entry| entry.entry_type.0 == 0);
        Ok(Self {
            action: VarInt(i32::from(removing_entire_list)),
            entries,
        })
    }
}

/// A single score line or entry on a scoreboard.
pub struct ScoreEntry {
    /// Unique scoreboard identifier for this entry.
    pub scoreboard_id: i64,
    /// Associated objective identifier.
    pub objective_name: String,
    /// Score value displayed alongside the entry.
    pub score: VarInt,
    /// Target type (1: player, 2: entity, 3: fake player).
    pub entry_type: VarInt, // 1 = player, 2 = entity, 3 = fake player
    /// Unique entity identifier when entry type is player or entity.
    pub entity_unique_id: i64,
    /// Custom label when entry type is a fake player.
    pub custom_name: String,
}

impl PacketWrite for ScoreEntry {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.write_for_type(writer, self.entry_type.0)
    }
}

impl PacketRead for ScoreEntry {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let entry_type = VarUInt::read(reader)?.0;
        let name = SCORE_ENTRY_VARIANTS
            .get(entry_type as usize)
            .ok_or_else(|| {
                Error::new(ErrorKind::InvalidData, "invalid Bedrock score entry type")
            })?;
        if String::read(reader)? != *name {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "invalid Bedrock score entry label",
            ));
        }
        let scoreboard_id = VarLong::read(reader)?.0;
        let objective_name = if entry_type == 0 {
            Option::<String>::read(reader)?.unwrap_or_default()
        } else {
            String::read(reader)?
        };
        let score = if entry_type == 0 {
            0
        } else {
            i32::read(reader)?
        };
        let entity_unique_id = match entry_type {
            1 | 2 => VarLong::read(reader)?.0,
            _ => 0,
        };
        let custom_name = if entry_type == 3 {
            String::read(reader)?
        } else {
            String::new()
        };
        Ok(Self {
            scoreboard_id,
            objective_name,
            score: VarInt(score),
            entry_type: VarInt(entry_type as i32),
            entity_unique_id,
            custom_name,
        })
    }
}

impl ScoreEntry {
    fn write_for_type<W: Write>(&self, writer: &mut W, entry_type: i32) -> Result<(), Error> {
        VarLong(self.scoreboard_id).write(writer)?;
        if entry_type == 0 {
            (!self.objective_name.is_empty()).write(writer)?;
            if !self.objective_name.is_empty() {
                self.objective_name.write(writer)?;
            }
            return Ok(());
        }
        if self.objective_name.is_empty() {
            " ".write(writer)?;
        } else {
            self.objective_name.write(writer)?;
        }
        self.score.0.write(writer)?;
        match entry_type {
            1 | 2 => {
                VarLong(self.entity_unique_id).write(writer)?;
            }
            3 => {
                if self.custom_name.is_empty() {
                    " ".write(writer)?;
                } else {
                    self.custom_name.write(writer)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_mixed_score_updates_and_removals() {
        let packet = CSetScore {
            action: VarInt(0),
            entries: vec![
                ScoreEntry {
                    scoreboard_id: 7,
                    objective_name: "board".into(),
                    score: VarInt(-42),
                    entry_type: VarInt(3),
                    entity_unique_id: 0,
                    custom_name: "line".into(),
                },
                ScoreEntry {
                    scoreboard_id: 5,
                    objective_name: String::new(),
                    score: VarInt(0),
                    entry_type: VarInt(0),
                    entity_unique_id: 0,
                    custom_name: String::new(),
                },
            ],
        };
        let mut body = Vec::new();
        packet.write(&mut body).unwrap();
        let mut remaining = body.as_slice();
        let decoded = CSetScore::read(&mut remaining).unwrap();
        assert!(remaining.is_empty());
        assert_eq!(decoded.entries[0].custom_name, "line");
        assert_eq!(decoded.entries[0].score, VarInt(-42));
        assert_eq!(decoded.entries[1].entry_type, VarInt(0));
        let mut reencoded = Vec::new();
        decoded.write(&mut reencoded).unwrap();
        assert_eq!(reencoded, body);
    }
}
