use std::io::{Error, Write};

use pumpkin_data::bedrock_jigsaw::JIGSAW_STRUCTURE_DATA;
use pumpkin_macros::packet;
use pumpkin_nbt::NbtCompound;

use crate::serial::{PacketRead, PacketWrite};

/// Transmits server-defined jigsaw structure generation rules, processors, and template pools.
#[packet(pumpkin_data::bedrock_packet::JIGSAW_STRUCTURE_DATA)]
pub struct CJigsawStructureData;

impl PacketWrite for CJigsawStructureData {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        writer.write_all(JIGSAW_STRUCTURE_DATA)
    }
}

/// Parsed structure, processor and template registries.
#[derive(PacketRead)]
#[packet(pumpkin_data::bedrock_packet::JIGSAW_STRUCTURE_DATA)]
pub struct CJigsawStructureDataPayload {
    pub data: NbtCompound,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jigsaw_structure_data_decodes_as_bedrock_network_nbt() {
        let mut encoded = Vec::new();
        CJigsawStructureData.write(&mut encoded).unwrap();

        let mut body = encoded.as_slice();
        let nbt = CJigsawStructureDataPayload::read(&mut body).unwrap().data;
        assert!(body.is_empty());

        assert_eq!(
            nbt.get_list("jigsaws").unwrap().len(),
            pumpkin_data::bedrock_jigsaw::JIGSAW_COUNT
        );
        assert_eq!(
            nbt.get_list("processors").unwrap().len(),
            pumpkin_data::bedrock_jigsaw::PROCESSOR_COUNT
        );
        assert_eq!(
            nbt.get_list("structure_sets").unwrap().len(),
            pumpkin_data::bedrock_jigsaw::STRUCTURE_SET_COUNT
        );
        assert_eq!(
            nbt.get_list("template_pools").unwrap().len(),
            pumpkin_data::bedrock_jigsaw::TEMPLATE_POOL_COUNT
        );
    }
}
