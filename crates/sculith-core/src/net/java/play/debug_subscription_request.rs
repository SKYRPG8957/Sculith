#[allow(clippy::wildcard_imports)]
use super::*;

impl JavaClient {
    pub fn handle_debug_subscription_request(
        &self,
        player: &Arc<Player>,
        packet: &sculith_protocol::java::server::play::SDebugSubscriptionRequest,
    ) {
        if player.permission_lvl.load() >= PermissionLvl::Two
            && packet.sample_type.0
                == sculith_protocol::java::server::play::SDebugSubscriptionRequest::TICK_TIME
        {
            player
                .subscribed_debug_sample
                .store(true, Ordering::Relaxed);
        }
    }
}
