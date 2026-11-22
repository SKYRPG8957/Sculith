use std::sync::{Arc, Mutex, Weak};

use pumpkin_util::math::vector2::Vector2;
use rustc_hash::FxHashMap;

use crate::{
    entity::{EntityBase, player::Player},
    server::Server,
};

use super::World;

/// Ready connections for an active extent; the ordered packets stay with the player.
#[derive(Default)]
pub(super) struct RegionalInput(Mutex<FxHashMap<u64, Weak<Player>>>);

impl RegionalInput {
    fn offer(&self, id: u64, player: &Arc<Player>) {
        if let Ok(mut ready) = self.0.try_lock() {
            ready.entry(id).or_insert_with(|| Arc::downgrade(player));
        }
    }

    pub(super) fn drain(
        &self,
        world: &World,
        server: &Server,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) {
        let Ok(mut queue) = self.0.try_lock() else {
            return;
        };
        let ready = std::mem::take(&mut *queue);
        drop(queue);
        if ready.is_empty() {
            return;
        }
        let Some(_native) = server.plugin_manager.try_without_native_code() else {
            return;
        };
        for (id, player) in ready {
            if let Some(player) = player.upgrade()
                && let Some(client) = player.client.bedrock()
                && client.drain_regional_input(&player, server, || {
                    player.can_simulate_in(world) && owns(player.get_entity().chunk_pos.load())
                })
            {
                self.offer(id, &player);
            }
        }
    }
}

impl World {
    pub(crate) fn offer_regional_input(&self, id: u64, player: &Arc<Player>) {
        if !player.can_simulate_in(self) {
            return;
        }
        let layout = self.region_layout.load();
        if let Some(owner) = layout.owner(player.get_entity().chunk_pos.load())
            && let Some(input) = layout.input(owner)
        {
            input.offer(id, player);
        }
    }
}
