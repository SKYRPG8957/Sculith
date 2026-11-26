pub mod light;
pub mod util;
pub mod v1_18;

pub use light::ChunkLightExt;

use pumpkin_data::packet::clientbound::play::LEVEL_CHUNK_WITH_LIGHT;
use pumpkin_world::chunk::ChunkData;
use sculith_protocol::ClientPacket;
use sculith_protocol::packet::MultiVersionJavaPacket;
use sculith_protocol::ser::WritingError;
use sculith_util::version::JavaMinecraftVersion;
use std::io::Write;

/// Sent by the server to provide the client with the full data for a chunk.
///
/// This includes heightmaps, the actual block and biome data (organized into sections),
/// block entities (like signs or chests), and the light level information for both
/// sky and block light.
pub struct CChunkData<'a>(pub &'a ChunkData);

impl MultiVersionJavaPacket for CChunkData<'_> {
    fn to_id(version: JavaMinecraftVersion) -> i32 {
        LEVEL_CHUNK_WITH_LIGHT.to_id(version)
    }
}

impl<'a> CChunkData<'a> {
    #[must_use]
    pub const fn new(chunk: &'a ChunkData) -> Self {
        Self(chunk)
    }
}

impl ClientPacket for CChunkData<'_> {
    fn write_packet_data(
        &self,
        write: impl Write,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        v1_18::write_chunk_data(self.0, write, version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::BLOCK_ENTITY_CUSTOM_DATA_TAG;
    use pumpkin_world::chunk::ChunkData;

    #[test]
    fn chunk_data_all_versions() {
        let chunk = ChunkData::empty(0, 0);
        let packet = CChunkData(&chunk);

        let versions = [JavaMinecraftVersion::V_26_3];

        for version in versions {
            let mut buf = Vec::new();
            let id = CChunkData::to_id(version);
            assert_ne!(id, -1, "Packet ID for version {version:?} must be valid");
            assert!(
                packet.write_packet_data(&mut buf, &version).is_ok(),
                "Failed to serialize chunk data for version {version:?}"
            );
            assert!(
                !buf.is_empty(),
                "Serialized buffer must not be empty for version {version:?}"
            );
        }
    }

    #[test]
    fn populated_chunk_data_all_versions() {
        let chunk = ChunkData::empty(0, 0);
        chunk
            .section
            .set_block_absolute_y(0, 64, 0, pumpkin_data::Block::STONE.default_state.id);
        chunk
            .section
            .set_block_absolute_y(1, 64, 1, pumpkin_data::Block::DIRT.default_state.id);

        let mut nbt = pumpkin_nbt::compound::NbtCompound::new();
        nbt.put_string("id", "minecraft:chest".to_string());
        let mut custom = pumpkin_nbt::compound::NbtCompound::new();
        let private_marker = "sculith_private_block_fixture";
        custom.put_string("unshared", private_marker.to_string());
        for tag in [
            BLOCK_ENTITY_CUSTOM_DATA_TAG,
            "PumpkinCustomData",
            "BukkitValues",
        ] {
            nbt.put_compound(tag, custom.clone());
        }
        chunk.pending_block_entities.lock().unwrap().insert(
            sculith_util::math::position::BlockPos(sculith_util::math::vector3::Vector3::new(
                0, 64, 0,
            )),
            nbt,
        );

        let packet = CChunkData(&chunk);

        let versions = [JavaMinecraftVersion::V_26_3];

        for version in versions {
            let mut buf = Vec::new();
            let id = CChunkData::to_id(version);
            assert_ne!(id, -1, "Packet ID for version {version:?} must be valid");
            assert!(
                packet.write_packet_data(&mut buf, &version).is_ok(),
                "Failed to serialize populated chunk data for version {version:?}"
            );
            assert!(
                !buf.is_empty(),
                "Serialized buffer must not be empty for version {version:?}"
            );
            assert!(
                !buf.windows(private_marker.len())
                    .any(|bytes| bytes == private_marker.as_bytes()),
                "Block entity plugin data must stay in storage for version {version:?}"
            );
        }
    }
}
