use std::sync::Arc;

use pumpkin_data::item_stack::ItemStack;
use sculith_util::GameMode;

use crate::plugin::inventory::inventory_creative::InventoryCreativeEvent;

use super::Player;

impl Player {
    pub(crate) fn allow_creative_change(self: &Arc<Self>, slot: i16, stack: &ItemStack) -> bool {
        if self.gamemode.load() != GameMode::Creative {
            return false;
        }
        let Some(server) = self.world().server.upgrade() else {
            return false;
        };
        let mut event = InventoryCreativeEvent::new(
            self.clone(),
            slot,
            stack.item.registry_key.to_string(),
            stack.item_count,
        );
        server.plugin_manager.fire_blocking(&server, &mut event);
        !event.cancelled && self.gamemode.load() == GameMode::Creative
    }
}
