use pumpkin_macros::packet;
use pumpkin_nbt::NbtCompound;

use crate::{
    codec::var_int::VarInt,
    serial::{PacketRead, PacketWrite},
};

/// Sent by the server to register custom and vanilla item identifiers with their component definitions.
#[derive(PacketWrite)]
#[packet(pumpkin_data::bedrock_packet::ITEM_REGISTRY)]
pub struct CItemRegistry {
    // https://mojang.github.io/bedrock-protocol-docs/docs/ItemRegistryPacket.html
    /// List of registered item definitions and their component NBT data.
    pub items: Vec<ItemData>,
}

/// Registration data for a single item type in the network item registry.
#[derive(PacketWrite)]
pub struct ItemData {
    /// String identifier for the item (e.g. `minecraft:diamond_sword`).
    pub item_name: String,
    /// Numerical runtime ID of the item.
    pub item_id: i16,
    /// Whether the item uses data-driven components.
    pub is_component_based: bool,

    // TODO: ItemVersion enum
    /// Format version of the item schema.
    pub item_version: VarInt,

    // Normally would be `Nbt`, but for simplicity elsewhere, this is preserialized (via `Nbt::write_bedrock`)
    /// Pre-serialized Bedrock NBT compound representing the item components.
    #[serial(no_prefix)]
    pub component_data: Vec<u8>,
}

/// The registered item definitions as parsed from the Bedrock wire payload.
#[derive(PacketRead)]
#[packet(pumpkin_data::bedrock_packet::ITEM_REGISTRY)]
pub struct CItemRegistryPayload {
    pub items: Vec<RegisteredItem>,
}

#[derive(PacketRead)]
pub struct RegisteredItem {
    pub item_name: String,
    pub item_id: i16,
    pub is_component_based: bool,
    pub item_version: VarInt,
    pub components: NbtCompound,
}

#[cfg(test)]
mod tests {
    use pumpkin_nbt::Nbt;

    use super::*;

    #[test]
    fn reads_multiple_items_with_an_nbt_document_each() {
        let item = |name: &str, id| {
            let mut nbt = NbtCompound::new();
            nbt.put_int("itemId", i32::from(id));
            ItemData {
                item_name: name.into(),
                item_id: id,
                is_component_based: true,
                item_version: VarInt(1),
                component_data: Nbt::from(nbt).write_bedrock().into(),
            }
        };
        let packet = CItemRegistry {
            items: vec![item("test:a", 1), item("test:b", 2)],
        };
        let mut bytes = Vec::new();
        packet.write(&mut bytes).unwrap();
        let mut body = bytes.as_slice();
        let decoded = CItemRegistryPayload::read(&mut body).unwrap();
        assert!(body.is_empty());
        assert_eq!(decoded.items[0].item_name, "test:a");
        assert_eq!(decoded.items[0].components.get_int("itemId"), Some(1));
        assert_eq!(decoded.items[1].item_name, "test:b");
        assert_eq!(decoded.items[1].components.get_int("itemId"), Some(2));
    }
}
