#[allow(clippy::wildcard_imports)]
use super::*;

impl BedrockClient {
    pub async fn handle_chat_command(
        &self,
        player: &Arc<Player>,
        server: &Arc<Server>,
        packet: SCommandRequest<'_>,
    ) {
        let command = packet.command.strip_prefix('/').unwrap_or(&packet.command);
        crate::server::player_commands::execute_player_command(server, player, command).await;
    }
}
