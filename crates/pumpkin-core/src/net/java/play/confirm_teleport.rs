use std::sync::{PoisonError, atomic::Ordering};

use sculith_protocol::java::server::play::SConfirmTeleport;
use sculith_util::text::TextComponent;

use crate::{entity::EntityBase, entity::player::Player, net::java::JavaClient};

impl JavaClient {
    pub fn handle_confirm_teleport(&self, player: &Player, confirm_teleport: &SConfirmTeleport) {
        let mut awaiting_teleport = player
            .awaiting_teleport
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match *awaiting_teleport {
            Some((id, position)) if id == confirm_teleport.teleport_id => {
                player.get_entity().set_pos(position);
                *awaiting_teleport = None;
            }
            None if player.teleport_id_count.load(Ordering::Relaxed)
                == confirm_teleport.teleport_id.0 =>
            {
                drop(awaiting_teleport);
                self.try_kick(&TextComponent::text(
                    "Send Teleport confirm, but we did not teleport",
                ));
            }
            // A newer teleport can be issued before the client confirms an earlier one.
            _ => {}
        }
    }
}
