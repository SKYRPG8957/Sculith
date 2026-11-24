#[allow(clippy::wildcard_imports)]
use super::*;

impl BedrockClient {
    pub fn handle_block_pick_request(&self, player: &Arc<Player>, packet: &SBlockPickRequest) {
        if !player.can_interact_with_block_at(&packet.position, 1.0) {
            return;
        }

        let world = player.world();
        let block = world.get_block(&packet.position);

        if block.item_id == 0 {
            return;
        }

        let Some(item) = pumpkin_data::item::Item::from_id(block.item_id) else {
            return;
        };
        let stack = ItemStack::new(1, item);
        player.pick_item(stack);
    }

    pub fn handle_actor_pick_request(&self, player: &Arc<Player>, packet: &SActorPickRequest) {
        if player.gamemode.load() != GameMode::Creative {
            return;
        }
        let Ok(id) = i32::try_from(packet.entity_unique_id) else {
            return;
        };
        let Some(target) = player.world().get_entity_by_id(id) else {
            return;
        };
        if !player.can_interact_with_entity(target.as_ref(), 1.0) {
            return;
        }

        let target_type = target.get_entity().entity_type;
        let item = pumpkin_data::entity::spawn_egg_ids()
            .iter()
            .find(|&&id| {
                pumpkin_data::entity::entity_from_egg(id)
                    .is_some_and(|entity_type| entity_type.id == target_type.id)
            })
            .and_then(|&id| pumpkin_data::item::Item::from_id(id));
        if let Some(item) = item {
            player.pick_item(ItemStack::new(1, item));
        }
    }
}
