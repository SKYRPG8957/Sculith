#[allow(clippy::wildcard_imports)]
use super::*;
use sculith_protocol::java::server::play::SChangeDifficulty;

impl JavaClient {
    pub fn handle_change_difficulty(
        &self,
        server: &Server,
        player: &Player,
        packet: &SChangeDifficulty,
    ) {
        if player.permission_lvl.load() < PermissionLvl::Two {
            warn!(
                "Player {} tried to change difficulty without required permissions",
                player.gameprofile.name
            );
            return;
        }

        if !server.set_scoped_difficulty(&player.world(), packet.difficulty, false) {
            warn!(
                "Player {} tried to change difficulty while difficulty is locked",
                player.gameprofile.name
            );
            return;
        }

        info!(
            "Player {} changed difficulty to {:?}",
            player.gameprofile.name, packet.difficulty
        );
    }
}
