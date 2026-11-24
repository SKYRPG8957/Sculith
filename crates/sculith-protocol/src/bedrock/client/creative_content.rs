use std::io::{Error, Read, Write};

use pumpkin_macros::packet;

use crate::{
    bedrock::network_item::NetworkItemDescriptor,
    codec::var_uint::VarUInt,
    serial::{PacketRead, PacketWrite},
};

/// Sent by the server to populate the creative inventory menu with item categories, groups, and items.
#[packet(pumpkin_data::bedrock_packet::CREATIVE_CONTENT)]
pub struct CCreativeContent<'a> {
    /// Creative tab groups with category icons and display names.
    pub groups: &'a [CreativeGroupInfoPayload],
    /// Individual creative item stack entries.
    pub entries: &'a [CreativeItemEntryPayload],
}

impl PacketWrite for CCreativeContent<'_> {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        VarUInt(self.groups.len() as u32).write(writer)?;
        for group in self.groups {
            group.write(writer)?;
        }

        VarUInt(self.entries.len() as u32).write(writer)?;
        for entry in self.entries {
            entry.write(writer)?;
        }
        Ok(())
    }
}

/// The creative inventory groups and items parsed from a packet body.
#[derive(PacketRead)]
#[packet(pumpkin_data::bedrock_packet::CREATIVE_CONTENT)]
pub struct CCreativeContentPayload {
    pub groups: Vec<CreativeGroupInfoPayload>,
    pub entries: Vec<CreativeItemEntryPayload>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PacketRead)]
#[repr(u8)]
pub enum CreativeCategory {
    All,
    Construction,
    Nature,
    Equipment,
    Items,
    ItemCommandOnly,
    Undefined,
}

impl PacketWrite for CreativeCategory {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        match self {
            Self::Construction
            | Self::Nature
            | Self::Equipment
            | Self::Items
            | Self::ItemCommandOnly => (*self as u8).write(writer),
            _ => Err(Error::other("Invalid CreativeCategory to send")),
        }
    }
}

pub struct CreativeGroupInfoPayload {
    pub creative_category: CreativeCategory,
    pub name: String,

    // TODO: update inventory
    pub group_icon_item: NetworkItemDescriptor,
}

impl PacketWrite for CreativeGroupInfoPayload {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.creative_category.write(writer)?;
        self.name.write(writer)?;
        self.group_icon_item.write_item_instance(writer)
    }
}

impl PacketRead for CreativeGroupInfoPayload {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        Ok(Self {
            creative_category: CreativeCategory::read(reader)?,
            name: String::read(reader)?,
            group_icon_item: NetworkItemDescriptor::read_item_instance(reader)?,
        })
    }
}

pub struct CreativeItemEntryPayload {
    pub id: VarUInt,

    // TODO: update inventory
    pub item: NetworkItemDescriptor,

    pub group_index: VarUInt,
}

impl PacketWrite for CreativeItemEntryPayload {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.id.write(writer)?;
        self.item.write_item_instance(writer)?;
        self.group_index.write(writer)
    }
}

impl PacketRead for CreativeItemEntryPayload {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        Ok(Self {
            id: VarUInt::read(reader)?,
            item: NetworkItemDescriptor::read_item_instance(reader)?,
            group_index: VarUInt::read(reader)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::var_int::VarInt;

    #[test]
    fn reads_a_group_icon_and_an_item_with_distinct_ids() {
        let groups = [CreativeGroupInfoPayload {
            creative_category: CreativeCategory::Items,
            name: "group".into(),
            group_icon_item: NetworkItemDescriptor::default(),
        }];
        let entries = [CreativeItemEntryPayload {
            id: VarUInt(128),
            item: NetworkItemDescriptor {
                id: VarInt(7),
                stack_size: 2,
                ..Default::default()
            },
            group_index: VarUInt(0),
        }];
        let mut bytes = Vec::new();
        CCreativeContent {
            groups: &groups,
            entries: &entries,
        }
        .write(&mut bytes)
        .unwrap();
        let mut body = bytes.as_slice();
        let decoded = CCreativeContentPayload::read(&mut body).unwrap();
        assert!(body.is_empty());
        assert_eq!(decoded.groups[0].name, "group");
        assert_eq!(decoded.groups[0].group_icon_item.id, VarInt(0));
        assert_eq!(decoded.entries[0].id, VarUInt(128));
        assert_eq!(decoded.entries[0].item.id, VarInt(7));
        assert_eq!(decoded.entries[0].item.stack_size, 2);
    }
}
