// Last verified for v2169

use crate::{
    bedrock::network_item::NetworkItemStackDescriptor, codec::var_ulong::VarULong,
    serial::PacketRead,
};
use pumpkin_macros::packet;

/// Sent by the client to update the item held or equipped in a mob or player slot.
#[derive(Debug, PacketRead)]
#[packet(pumpkin_data::bedrock_packet::MOB_EQUIPMENT)]
pub struct SMobEquipment {
    /// Runtime entity ID of the mob or player changing equipment.
    pub entity_runtime_id: VarULong,
    /// Item descriptor being equipped.
    pub item: NetworkItemStackDescriptor,
    /// Slot index in the container.
    pub slot: u8,
    /// Currently selected hotbar slot.
    pub selected_slot: u8,
    /// Container identifier (e.g. inventory or armor).
    pub container_id: u8,
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use crate::serial::PacketWrite;

    use super::*;

    #[test]
    fn mob_equipment_reads_network_item_stack_descriptor() {
        let item = NetworkItemStackDescriptor {
            id: 1,
            stack_size: 2,
            extra_data: vec![0; 10],
            ..Default::default()
        };
        let mut encoded = Vec::new();
        VarULong(42).write(&mut encoded).unwrap();
        item.write(&mut encoded).unwrap();
        [3u8, 4, 5].write(&mut encoded).unwrap();

        let mut reader = Cursor::new(encoded);
        let packet = SMobEquipment::read(&mut reader).unwrap();

        assert_eq!(packet.entity_runtime_id.0, 42);
        assert_eq!(packet.item.id, item.id);
        assert_eq!(packet.item.stack_size, item.stack_size);
        assert_eq!(packet.item.extra_data, item.extra_data);
        assert_eq!(packet.slot, 3);
        assert_eq!(packet.selected_slot, 4);
        assert_eq!(packet.container_id, 5);
        assert_eq!(reader.position(), reader.get_ref().len() as u64);
    }
}
