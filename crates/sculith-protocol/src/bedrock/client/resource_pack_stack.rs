// Last verified for v2169

use crate::{
    bedrock::client::start_game::Experiments,
    serial::{PacketRead, PacketWrite},
};
use pumpkin_macros::packet;

/// Identifies a specific pack instance and sub-pack layer in the active resource pack stack.
#[derive(PacketRead, PacketWrite)]
pub struct PackInstanceId {
    /// UUID string of the resource pack.
    pub pack_id: String,
    /// Semantic version string.
    pub version: String,
    /// Sub-pack configuration name.
    pub sub_pack_name: String,
}

/// Sent by the server to define the order in which resource and behavior packs are applied by the client.
#[derive(PacketRead, PacketWrite)]
#[packet(pumpkin_data::bedrock_packet::RESOURCE_PACK_STACK)]
pub struct CResourcePackStackPacket {
    /// If true, textures from the resource packs are strictly required to join.
    pub texture_pack_required: bool,
    /// Ordered list of packs to apply in bottom-to-top priority.
    pub texture_pack_list: Vec<PackInstanceId>,
    /// Base vanilla version of the game.
    pub base_game_version: String,
    /// Experimental gameplay feature toggles enabled for this world.
    pub experiments: Experiments,
    /// Whether to load editor mode packs.
    pub include_editor_packs: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bedrock::client::start_game::ExperimentToggle;

    #[test]
    fn preserves_an_experiment_with_the_fixed_width_count() {
        let stack = CResourcePackStackPacket {
            texture_pack_required: true,
            texture_pack_list: vec![],
            base_game_version: "1.26.60".into(),
            experiments: Experiments {
                toggles: vec![ExperimentToggle {
                    name: "sample".into(),
                    enabled: true,
                }],
                experiments_ever_toggled: true,
            },
            include_editor_packs: false,
        };
        let mut body = Vec::new();
        stack.write(&mut body).unwrap();
        let mut body = body.as_slice();
        let decoded = CResourcePackStackPacket::read(&mut body).unwrap();
        assert!(body.is_empty());
        assert_eq!(decoded.base_game_version, "1.26.60");
        assert_eq!(decoded.experiments.toggles[0].name, "sample");
        assert!(decoded.experiments.toggles[0].enabled);
        assert!(decoded.experiments.experiments_ever_toggled);
    }
}
