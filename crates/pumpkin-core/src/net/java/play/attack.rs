#[allow(clippy::wildcard_imports)]
use super::*;

impl JavaClient {
    pub fn handle_attack(&self, player: &Arc<Player>, attack: &SAttack, server: &Arc<Server>) {
        if !player.has_client_loaded() {
            return;
        }
        player.update_last_action_time();
        let entity_id = attack.entity_id;
        let player_entity = &player.get_entity();
        let world = player_entity.world.load_full();

        if entity_id.0 == player.entity_id() {
            self.try_kick(&TextComponent::translate_cross(
                translation::java::MULTIPLAYER_DISCONNECT_INVALID_ENTITY_ATTACKED,
                translation::java::MULTIPLAYER_DISCONNECT_INVALID_ENTITY_ATTACKED,
                [],
            ));
            return;
        }

        let Some(target) = world.get_entity_by_id(entity_id.0) else {
            self.try_kick(&TextComponent::translate_cross(
                translation::java::MULTIPLAYER_DISCONNECT_INVALID_ENTITY_ATTACKED,
                translation::java::MULTIPLAYER_DISCONNECT_INVALID_ENTITY_ATTACKED,
                [],
            ));
            return;
        };
        if !player.can_interact_with_entity(target.as_ref(), 1.0) {
            return;
        }
        let mut event = PlayerInteractEntityEvent::new(
            player,
            target,
            ActionType::Attack,
            None,
            player_entity.is_sneaking(),
        );
        server.plugin_manager.fire_blocking(server, &mut event);
        if !event.cancelled && event.action == ActionType::Attack {
            player.attack_from_client(server, &world, &event.target);
        }
    }
}
