#[allow(clippy::wildcard_imports)]
use super::*;

impl BedrockClient {
    pub fn handle_player_block_action(
        &self,
        player: &Arc<Player>,
        server: &Arc<Server>,
        packet: &pumpkin_protocol::bedrock::server::player_auth_input::PlayerBlockAction,
    ) {
        self.apply_player_action(player, server, packet.action, packet.block_pos);
    }
}
