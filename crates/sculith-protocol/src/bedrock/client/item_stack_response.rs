use std::io::{Error, ErrorKind, Read, Write};

use crate::{
    bedrock::network_item::FullContainerName,
    codec::{var_int::VarInt, var_uint::VarUInt},
    serial::{PacketRead, PacketWrite},
};
use pumpkin_macros::packet;

/// Slot update information resulting from an approved item stack request.
#[derive(Debug, Clone)]
pub struct ItemStackResponseSlotInfo {
    /// Slot index requested by the client.
    pub requested_slot: u8,
    /// Destination slot index allocated by the server.
    pub slot: u8,
    /// Item count in the slot.
    pub amount: u8,
    /// Unique network stack ID assigned to the slot's item stack.
    pub item_stack_net_id: VarInt,
    /// Custom display name assigned to the item.
    pub custom_name: String,
    /// Filtered custom display name.
    pub filtered_custom_name: String,
    /// Durability correction delta.
    pub durability_correction: VarInt,
}

impl PacketWrite for ItemStackResponseSlotInfo {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        if !(-32768..=32767).contains(&self.durability_correction.0) {
            return Err(Error::new(
                std::io::ErrorKind::InvalidInput,
                "durability correction must fit in an i16",
            ));
        }
        self.requested_slot.write(writer)?;
        self.slot.write(writer)?;
        self.amount.write(writer)?;
        (self.item_stack_net_id.0 > 0).write(writer)?;
        if self.item_stack_net_id.0 > 0 {
            self.item_stack_net_id.write(writer)?;
        }
        self.custom_name.write(writer)?;
        self.filtered_custom_name.write(writer)?;
        self.durability_correction.write(writer)
    }
}

impl PacketRead for ItemStackResponseSlotInfo {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let requested_slot = u8::read(reader)?;
        let slot = u8::read(reader)?;
        let amount = u8::read(reader)?;
        let item_stack_net_id = Option::<VarInt>::read(reader)?.unwrap_or(VarInt(0));
        let custom_name = String::read(reader)?;
        let filtered_custom_name = String::read(reader)?;
        let durability_correction = VarInt::read(reader)?;
        if i16::try_from(durability_correction.0).is_err() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "durability correction must fit in an i16",
            ));
        }
        Ok(Self {
            requested_slot,
            slot,
            amount,
            item_stack_net_id,
            custom_name,
            filtered_custom_name,
            durability_correction,
        })
    }
}

#[derive(PacketRead, PacketWrite, Debug, Clone)]
pub struct ItemStackResponseContainerInfo {
    pub full_container_name: FullContainerName,
    pub slots: Vec<ItemStackResponseSlotInfo>,
}

/// Status result and slot updates for an individual item stack request.
#[derive(Debug, Clone)]
pub struct ItemStackResponseInfo {
    // TODO: proper enum
    /// Status code of the response (`0` = Success, `1` = Error).
    pub result: u8, // 0 = SUCCESS, 1 = ERROR
    /// Request ID matching the client's `ItemStackRequest`.
    pub client_request_id: VarInt,
    /// Container slot updates applied as a consequence of the request.
    pub containers: Vec<ItemStackResponseContainerInfo>,
}

impl PacketWrite for ItemStackResponseInfo {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.result.write(writer)?;
        self.client_request_id.write(writer)?;
        (!self.containers.is_empty()).write(writer)?;
        if !self.containers.is_empty() {
            VarUInt(self.containers.len() as u32).write(writer)?;
            for info in &self.containers {
                info.write(writer)?;
            }
        }
        Ok(())
    }
}

impl PacketRead for ItemStackResponseInfo {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        Ok(Self {
            result: u8::read(reader)?,
            client_request_id: VarInt::read(reader)?,
            containers: Option::<Vec<_>>::read(reader)?.unwrap_or_default(),
        })
    }
}

/// Sent by the server to approve or reject item stack requests and sync modified slots.
#[derive(Debug, Clone)]
#[packet(pumpkin_data::bedrock_packet::ITEM_STACK_RESPONSE)]
pub struct CItemStackResponse {
    /// List of response statuses corresponding to client requests.
    pub responses: Vec<ItemStackResponseInfo>,
}

impl PacketWrite for CItemStackResponse {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        if self.responses.len() > 4096 {
            return Err(Error::new(
                std::io::ErrorKind::InvalidInput,
                "item stack response count exceeds 4096",
            ));
        }
        VarUInt(self.responses.len() as u32).write(writer)?;
        for response in &self.responses {
            response.write(writer)?;
        }
        Ok(())
    }
}

impl PacketRead for CItemStackResponse {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let count = VarUInt::read(reader)?.0;
        if count > 4096 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "item stack response count exceeds 4096",
            ));
        }
        let mut responses = Vec::new();
        for _ in 0..count {
            responses.push(ItemStackResponseInfo::read(reader)?);
        }
        Ok(Self { responses })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_out_of_range_durability_correction() {
        let slot = ItemStackResponseSlotInfo {
            requested_slot: 0,
            slot: 0,
            amount: 1,
            item_stack_net_id: VarInt(1),
            custom_name: String::new(),
            filtered_custom_name: String::new(),
            durability_correction: VarInt(32768),
        };
        assert!(slot.write(&mut Vec::new()).is_err());
    }

    #[test]
    fn round_trips_failed_response_without_containers() {
        let response = CItemStackResponse {
            responses: vec![ItemStackResponseInfo {
                result: 1,
                client_request_id: VarInt(-7),
                containers: vec![],
            }],
        };
        let mut bytes = Vec::new();
        response.write(&mut bytes).unwrap();
        assert_eq!(bytes, [1, 1, 13, 0]);
        let mut body = bytes.as_slice();
        let decoded = CItemStackResponse::read(&mut body).unwrap();
        assert!(body.is_empty());
        assert_eq!(decoded.responses[0].client_request_id, VarInt(-7));
        assert!(decoded.responses[0].containers.is_empty());
    }
}
