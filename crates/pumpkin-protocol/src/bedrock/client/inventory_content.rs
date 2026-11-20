// Last verified for v2169

use pumpkin_macros::packet;

use crate::{
    bedrock::network_item::{FullContainerName, NetworkItemStackDescriptor},
    codec::var_uint::VarUInt,
    serial::{PacketRead, PacketWrite},
};

/// Sent by the server to synchronize the entire contents of an inventory or container window.
#[derive(PacketRead, PacketWrite)]
#[packet(pumpkin_data::bedrock_packet::INVENTORY_CONTENT)]
pub struct CInventoryContent {
    /// Window ID of the container being populated.
    pub container_id: VarUInt,
    /// Array of item stacks filling every slot of the container.
    pub slots: Vec<NetworkItemStackDescriptor>,
    /// Full container name descriptor with dynamic session ID.
    pub full_container_name: FullContainerName,
    /// Storage item descriptor (e.g. shulker box item if inspecting inside a shulker).
    pub storage_item: NetworkItemStackDescriptor,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bedrock::network_item::ContainerName;
    use std::num::NonZero;

    #[test]
    fn round_trips_inventory_slots_with_stack_and_container_ids() {
        let content = CInventoryContent {
            container_id: VarUInt(2),
            slots: vec![NetworkItemStackDescriptor {
                id: 12,
                stack_size: 3,
                net_id: NonZero::new(27),
                ..Default::default()
            }],
            full_container_name: FullContainerName {
                container_name: ContainerName::Inventory,
                dynamic_id: Some(42),
            },
            storage_item: NetworkItemStackDescriptor::default(),
        };
        let mut body = Vec::new();
        content.write(&mut body).unwrap();
        let mut body = body.as_slice();
        let decoded = CInventoryContent::read(&mut body).unwrap();
        assert!(body.is_empty());
        assert_eq!(decoded.container_id, VarUInt(2));
        assert_eq!(decoded.full_container_name, content.full_container_name);
        assert_eq!(decoded.slots[0].id, 12);
        assert_eq!(decoded.slots[0].stack_size, 3);
        assert_eq!(decoded.slots[0].net_id, NonZero::new(27));
    }
}
