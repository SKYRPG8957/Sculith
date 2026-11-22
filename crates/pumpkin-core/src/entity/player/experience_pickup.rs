use std::sync::Arc;

use crate::server::{Server, TickDeadline};

use super::Player;

impl Player {
    pub(crate) fn begin_experience_pickup(self: &Arc<Self>) -> bool {
        let Ok(mut delay) = self.experience_pick_up_delay.try_lock() else {
            return false;
        };
        if *delay > 0 {
            return false;
        }
        *delay = 2;
        if let Some(client) = self
            .client
            .bedrock()
            .filter(|client| client.is_current_protocol())
        {
            client.start_experience_pickup_clock();
            drop(delay);
            self.world()
                .offer_regional_experience_pickup(client.id, self);
        }
        true
    }

    pub(super) fn tick_experience_pickup(&self, server: &Server, deadline: Option<TickDeadline>) {
        let Ok(mut delay) = self.experience_pick_up_delay.try_lock() else {
            return;
        };
        if *delay == 0 {
            return;
        }
        let Some(client) = self
            .client
            .bedrock()
            .filter(|client| client.is_current_protocol())
        else {
            *delay -= 1;
            return;
        };
        if deadline.is_some()
            && let Some(_native) = server.plugin_manager.try_without_native_code()
        {
            let world = self.world();
            if let Some(player) = world.get_player_by_uuid(self.gameprofile.id)
                && std::ptr::eq(player.as_ref(), self)
                && world.offer_regional_experience_pickup(client.id, &player)
            {
                return;
            }
        }
        client.tick_experience_pickup_delay(&mut delay, deadline);
    }
}
