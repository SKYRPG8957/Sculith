#[allow(clippy::wildcard_imports)]
use super::*;

impl BedrockClient {
    pub fn handle_container_close(&self, player: &Arc<Player>, packet: &SContainerClose) {
        if packet.container_id == 0 || packet.container_id == 0xff {
            self.inventory_opened.store(false, Ordering::Relaxed);
        }
        player.on_handled_screen_closed();

        self.try_enqueue_client_packet(&SContainerClose {
            container_id: packet.container_id,
            container_type: packet.container_type,
            server_initiated_close: false,
        });

        self.resend_player_inventory(player);
    }
}
