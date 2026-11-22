#[allow(clippy::wildcard_imports)]
use super::*;

impl JavaClient {
    pub async fn handle_chat_command(
        &self,
        player: &Arc<Player>,
        server: &Arc<Server>,
        command: &SChatCommand<'_>,
    ) {
        // Keep a leading slash: a client typing `//wand` (WorldEdit style) sends `/wand`, and the
        // dispatcher decides itself whether that slash belongs to the command name or is a prefix.
        crate::server::player_commands::execute_player_command(server, player, command.command)
            .await;
    }
}
