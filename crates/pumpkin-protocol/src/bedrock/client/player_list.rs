use crate::{
    codec::{var_long::VarLong, var_uint::VarUInt},
    serial::{PacketRead, PacketWrite},
};
use bytes::Bytes;
use pumpkin_macros::packet;
use std::io::{Error, ErrorKind, Read, Write};
use uuid::Uuid;

use super::common::BuildPlatform;

const WIDE_SKIN_RESOURCE_PATCH: &[u8] = br#"{"geometry":{"default":"geometry.humanoid.custom"}}"#;
const SLIM_SKIN_RESOURCE_PATCH: &[u8] =
    br#"{"geometry":{"default":"geometry.humanoid.customSlim"}}"#;
const DEFAULT_SKIN_GEOMETRY: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/bedrock/player_geometry.json"
));

/// Sent by the server to add or remove players from the in-game pause screen player list.
#[packet(pumpkin_data::bedrock_packet::PLAYER_LIST)]
pub struct CPlayerList {
    /// Action type (`ACTION_ADD` = 0 to add, `ACTION_REMOVE` = 1 to remove).
    pub action: u8,
    /// Player entries being added or removed.
    pub entries: Vec<PlayerListEntry>,
}

impl PacketWrite for CPlayerList {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        VarUInt(self.entries.len() as u32).write(writer)?;
        for entry in &self.entries {
            match self.action {
                Self::ACTION_ADD => {
                    VarUInt(1).write(writer)?;
                    Self::ACTION_ADD.write(writer)?;
                    entry.write(writer)?;
                }
                Self::ACTION_REMOVE => {
                    VarUInt(0).write(writer)?;
                    Self::ACTION_REMOVE.write(writer)?;
                    entry.uuid.write(writer)?;
                }
                _ => return Err(Error::other("Invalid PlayerList action")),
            }
        }
        Ok(())
    }
}

impl CPlayerList {
    pub const ACTION_ADD: u8 = 0;
    pub const ACTION_REMOVE: u8 = 1;
}

/// Player list additions and removals read from the wire.
#[derive(PacketRead)]
#[packet(pumpkin_data::bedrock_packet::PLAYER_LIST)]
pub struct CPlayerListPayload {
    pub entries: Vec<PlayerListChange>,
}

pub enum PlayerListChange {
    Add(Box<PlayerListEntry>),
    Remove(Uuid),
}

impl PacketRead for PlayerListChange {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        match (VarUInt::read(reader)?.0, u8::read(reader)?) {
            (0, CPlayerList::ACTION_REMOVE) => Ok(Self::Remove(Uuid::read(reader)?)),
            (1, CPlayerList::ACTION_ADD) => Ok(Self::Add(Box::new(PlayerListEntry::read(reader)?))),
            _ => Err(Error::new(
                ErrorKind::InvalidData,
                "invalid Bedrock player list action",
            )),
        }
    }
}

pub struct PlayerListEntry {
    pub uuid: Uuid,
    pub entity_unique_id: VarLong,
    pub username: String,
    pub xuid: String,
    pub platform_chat_id: String,
    pub build_platform: BuildPlatform,
    pub skin: Skin,
    pub is_teacher: bool,
    pub is_host: bool,
    pub is_sub_client: bool,
    pub player_color: [u8; 4], // ARGB
}

impl PacketRead for PlayerListEntry {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let uuid = Uuid::read(reader)?;
        let entity_unique_id = VarLong::read(reader)?;
        let username = String::read(reader)?;
        let xuid = String::read(reader)?;
        let play_fab_id = String::read(reader)?;
        let platform_chat_id = String::read(reader)?;
        let build_platform = BuildPlatform::read(reader)?;
        let mut skin = Skin::read(reader)?;
        skin.play_fab_id = play_fab_id;
        Ok(Self {
            uuid,
            entity_unique_id,
            username,
            xuid,
            platform_chat_id,
            build_platform,
            skin,
            is_teacher: bool::read(reader)?,
            is_host: bool::read(reader)?,
            is_sub_client: bool::read(reader)?,
            player_color: u32::read(reader)?.to_be_bytes(),
        })
    }
}

impl PacketWrite for PlayerListEntry {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.uuid.write(writer)?;
        self.entity_unique_id.write(writer)?;
        self.username.write(writer)?;
        self.xuid.write(writer)?;
        self.skin.play_fab_id.write(writer)?;
        self.platform_chat_id.write(writer)?;
        self.build_platform.write(writer)?;
        self.skin.write(writer)?;
        self.is_teacher.write(writer)?;
        self.is_host.write(writer)?;
        self.is_sub_client.write(writer)?;
        u32::from_be_bytes(self.player_color).write(writer)
    }
}

#[derive(Clone)]
pub struct Skin {
    pub skin_id: String,
    pub play_fab_id: String,
    pub resource_patch: Vec<u8>,
    pub image_width: u32,
    pub image_height: u32,
    pub skin_data: Vec<u8>,
    pub animations: Vec<SkinAnimation>,
    pub cape_width: u32,
    pub cape_height: u32,
    pub cape_data: Vec<u8>,
    pub geometry_data: Vec<u8>,
    pub animation_data: Vec<u8>,
    pub geometry_data_engine_version: Vec<u8>,
    pub cape_id: String,
    pub full_id: String,
    pub arm_size: String,
    pub skin_color: String,
    pub persona_pieces: Vec<PersonaPiece>,
    pub piece_tint_colors: Vec<PieceTintColor>,
    pub is_premium: bool,
    pub is_persona: bool,
    pub persona_cape_on_classic: bool,
    pub is_primary_user: bool,
    pub override_appearance: bool,
    pub is_trusted: bool,
    pub profile_hash: String,
}

impl Skin {
    #[must_use]
    pub fn steve() -> Self {
        Self {
            skin_id: "Standard_Custom".to_string(),
            play_fab_id: String::new(),
            resource_patch: WIDE_SKIN_RESOURCE_PATCH.to_vec(),
            image_width: 64,
            image_height: 64,
            // 64 * 64 * 4 = 16384 bytes of raw RGBA data
            // Fill with 255 so the skin is visible (solid white) instead of invisible (transparent)
            skin_data: vec![255; 16384],
            animations: Vec::new(),
            cape_width: 0,
            cape_height: 0,
            cape_data: Vec::new(),
            geometry_data: DEFAULT_SKIN_GEOMETRY.to_vec(),
            animation_data: Vec::new(),
            geometry_data_engine_version: b"1.26.40".to_vec(),
            cape_id: String::new(),
            full_id: "Standard_Custom".to_string(),
            arm_size: "wide".to_string(),
            skin_color: "#0".to_string(),
            persona_pieces: Vec::new(),
            piece_tint_colors: Vec::new(),
            is_premium: true,
            is_persona: false,
            persona_cape_on_classic: false,
            is_primary_user: false,
            override_appearance: true,
            is_trusted: true,
            profile_hash: String::new(),
        }
    }

    /// Selects the standard wide or slim player geometry while preserving the
    /// rest of the serialized skin.
    pub fn set_slim(&mut self, slim: bool) {
        self.arm_size = if slim { "slim" } else { "wide" }.to_string();
        self.resource_patch = if slim {
            SLIM_SKIN_RESOURCE_PATCH
        } else {
            WIDE_SKIN_RESOURCE_PATCH
        }
        .to_vec();
    }
}

impl PacketWrite for Skin {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.skin_id.write(writer)?;
        VarUInt(self.resource_patch.len() as u32).write(writer)?;
        writer.write_all(&self.resource_patch)?;
        self.image_width.write(writer)?;
        self.image_height.write(writer)?;
        VarUInt(self.skin_data.len() as u32).write(writer)?;
        writer.write_all(&self.skin_data)?;
        VarUInt(self.animations.len() as u32).write(writer)?;
        for anim in &self.animations {
            anim.write(writer)?;
        }
        self.cape_width.write(writer)?;
        self.cape_height.write(writer)?;
        VarUInt(self.cape_data.len() as u32).write(writer)?;
        writer.write_all(&self.cape_data)?;
        VarUInt(self.geometry_data.len() as u32).write(writer)?;
        writer.write_all(&self.geometry_data)?;
        VarUInt(self.geometry_data_engine_version.len() as u32).write(writer)?;
        writer.write_all(&self.geometry_data_engine_version)?;
        VarUInt(self.animation_data.len() as u32).write(writer)?;
        writer.write_all(&self.animation_data)?;
        self.cape_id.write(writer)?;
        self.full_id.write(writer)?;
        u8::from(!self.arm_size.eq_ignore_ascii_case("slim")).write(writer)?;
        parse_color(&self.skin_color).write(writer)?;
        VarUInt(self.persona_pieces.len() as u32).write(writer)?;
        for piece in &self.persona_pieces {
            piece.write(writer)?;
        }
        VarUInt(self.piece_tint_colors.len() as u32).write(writer)?;
        for color in &self.piece_tint_colors {
            color.write(writer)?;
        }
        self.is_premium.write(writer)?;
        self.is_persona.write(writer)?;
        self.persona_cape_on_classic.write(writer)?;
        self.is_primary_user.write(writer)?;
        self.override_appearance.write(writer)?;
        self.is_trusted.to_string().write(writer)?;
        self.profile_hash.write(writer)
    }
}

impl PacketRead for Skin {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let skin_id = String::read(reader)?;
        let resource_patch = Bytes::read(reader)?.into();
        let image_width = u32::read(reader)?;
        let image_height = u32::read(reader)?;
        let skin_data = Bytes::read(reader)?.into();
        let animations = Vec::read(reader)?;
        let cape_width = u32::read(reader)?;
        let cape_height = u32::read(reader)?;
        let cape_data = Bytes::read(reader)?.into();
        let geometry_data = Bytes::read(reader)?.into();
        let geometry_data_engine_version = Bytes::read(reader)?.into();
        let animation_data = Bytes::read(reader)?.into();
        let cape_id = String::read(reader)?;
        let full_id = String::read(reader)?;
        let arm_size = match u8::read(reader)? {
            0 => "slim".into(),
            1 => "wide".into(),
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "invalid Bedrock skin arm size",
                ));
            }
        };
        let skin_color = format!("#{:08x}", u32::read(reader)?);
        let persona_pieces = Vec::read(reader)?;
        let piece_tint_colors = Vec::read(reader)?;
        let is_premium = bool::read(reader)?;
        let is_persona = bool::read(reader)?;
        let persona_cape_on_classic = bool::read(reader)?;
        let is_primary_user = bool::read(reader)?;
        let override_appearance = bool::read(reader)?;
        let is_trusted = String::read(reader)?
            .parse()
            .map_err(|_| Error::new(ErrorKind::InvalidData, "invalid Bedrock skin trust flag"))?;
        let profile_hash = String::read(reader)?;
        Ok(Self {
            skin_id,
            play_fab_id: String::new(),
            resource_patch,
            image_width,
            image_height,
            skin_data,
            animations,
            cape_width,
            cape_height,
            cape_data,
            geometry_data,
            animation_data,
            geometry_data_engine_version,
            cape_id,
            full_id,
            arm_size,
            skin_color,
            persona_pieces,
            piece_tint_colors,
            is_premium,
            is_persona,
            persona_cape_on_classic,
            is_primary_user,
            override_appearance,
            is_trusted,
            profile_hash,
        })
    }
}

#[derive(Clone)]
pub struct SkinAnimation {
    pub image_width: u32,
    pub image_height: u32,
    pub image_data: Vec<u8>,
    pub animation_type: u32,
    pub frames: f32,
    pub expression_type: u32,
}

impl PacketWrite for SkinAnimation {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.image_width.write(writer)?;
        self.image_height.write(writer)?;
        VarUInt(self.image_data.len() as u32).write(writer)?;
        writer.write_all(&self.image_data)?;
        VarUInt(self.animation_type).write(writer)?;
        self.frames.write(writer)?;
        VarUInt(self.expression_type).write(writer)
    }
}

impl PacketRead for SkinAnimation {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        Ok(Self {
            image_width: u32::read(reader)?,
            image_height: u32::read(reader)?,
            image_data: Bytes::read(reader)?.into(),
            animation_type: VarUInt::read(reader)?.0,
            frames: f32::read(reader)?,
            expression_type: VarUInt::read(reader)?.0,
        })
    }
}

#[derive(Clone, PacketRead, PacketWrite)]
pub struct PersonaPiece {
    pub piece_id: String,
    pub piece_type: i32,
    pub pack_id: Uuid,
    pub is_default: bool,
    pub product_id: String,
}

#[derive(Clone, PacketRead)]
pub struct PieceTintColor {
    pub piece_type: String,
    pub colors: [i32; 4],
}

impl PacketWrite for PieceTintColor {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        let piece_type = if self.piece_type == "persona_hand" {
            "hands"
        } else {
            self.piece_type
                .strip_prefix("persona_")
                .unwrap_or(&self.piece_type)
        };
        piece_type.write(writer)?;
        for color in self.colors {
            color.write(writer)?;
        }
        Ok(())
    }
}

fn parse_color(color: &str) -> i32 {
    let value = color.trim_start_matches('#');
    u32::from_str_radix(value, 16).unwrap_or_default() as i32
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_SKIN_GEOMETRY, SLIM_SKIN_RESOURCE_PATCH, Skin, WIDE_SKIN_RESOURCE_PATCH};

    #[test]
    fn reads_player_list_addition_with_skin_and_removal_with_uuid() {
        use super::*;

        let uuid = Uuid::from_u128(0x1234_5678_9abc_def0_3141_5926_5358_9793);
        for action in [CPlayerList::ACTION_ADD, CPlayerList::ACTION_REMOVE] {
            let packet = CPlayerList {
                action,
                entries: vec![PlayerListEntry {
                    uuid,
                    entity_unique_id: VarLong(18),
                    username: "Tester".into(),
                    xuid: String::new(),
                    platform_chat_id: String::new(),
                    build_platform: BuildPlatform::Uwp,
                    skin: Skin::steve(),
                    is_teacher: false,
                    is_host: false,
                    is_sub_client: false,
                    player_color: [255, 9, 8, 7],
                }],
            };
            let mut encoded = Vec::new();
            packet.write(&mut encoded).unwrap();
            let mut body = encoded.as_slice();
            let decoded = CPlayerListPayload::read(&mut body).unwrap();
            assert!(body.is_empty());
            match (&decoded.entries[0], action) {
                (PlayerListChange::Remove(id), CPlayerList::ACTION_REMOVE) => assert_eq!(*id, uuid),
                (PlayerListChange::Add(entry), CPlayerList::ACTION_ADD) => {
                    assert_eq!(entry.uuid, uuid);
                    assert_eq!(entry.username, "Tester");
                    assert_eq!(entry.player_color, [255, 9, 8, 7]);
                    assert_eq!(entry.skin.skin_data.len(), 16_384);
                }
                _ => panic!("unexpected player list action"),
            }
        }
    }

    #[test]
    fn fallback_skin_contains_the_geometry_it_references() {
        let skin = Skin::steve();

        assert_eq!(skin.resource_patch, WIDE_SKIN_RESOURCE_PATCH);
        assert_eq!(skin.geometry_data, DEFAULT_SKIN_GEOMETRY);
        assert!(
            String::from_utf8_lossy(&skin.geometry_data)
                .contains(r#""identifier":"geometry.humanoid.custom""#)
        );
        assert!(skin.override_appearance);
        assert!(skin.is_trusted);
    }

    #[test]
    fn slim_fallback_skin_references_the_slim_geometry() {
        let mut skin = Skin::steve();
        skin.set_slim(true);

        assert_eq!(skin.arm_size, "slim");
        assert_eq!(skin.resource_patch, SLIM_SKIN_RESOURCE_PATCH);
        assert!(
            String::from_utf8_lossy(&skin.geometry_data)
                .contains(r#""identifier":"geometry.humanoid.customSlim""#)
        );
    }
}
