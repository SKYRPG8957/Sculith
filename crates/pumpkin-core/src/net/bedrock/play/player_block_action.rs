#[allow(clippy::wildcard_imports)]
use super::*;

impl BedrockClient {
    pub fn handle_player_block_action(
        &self,
        player: &Arc<Player>,
        server: &Arc<Server>,
        packet: &sculith_protocol::bedrock::server::player_auth_input::PlayerBlockAction,
    ) {
        let mut spin_attack_prediction = None;
        self.apply_player_action(
            player,
            server,
            packet.action,
            packet.block_pos,
            &mut spin_attack_prediction,
        );
        self.reconcile_spin_attack_prediction(player, spin_attack_prediction);
    }
}
