use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use pumpkin_util::math::vector2::Vector2;
use rustc_hash::FxHashMap;

use crate::{
    entity::{EntityBase, player::Player},
    server::Server,
};

use super::{RegionChunks, RegionLayout, World};

type ReadyConnections = FxHashMap<u64, Weak<Player>>;

/// Ready connections for an active extent; the ordered packets stay with the player.
pub(super) struct RegionalInput(Mutex<Option<ReadyConnections>>);

impl Default for RegionalInput {
    fn default() -> Self {
        Self(Mutex::new(Some(FxHashMap::default())))
    }
}

impl RegionalInput {
    fn ready(&self) -> MutexGuard<'_, Option<ReadyConnections>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn offer(&self, id: u64, player: &Arc<Player>) -> bool {
        let mut queue = self.ready();
        let Some(ready) = queue.as_mut() else {
            return false;
        };
        ready.entry(id).or_insert_with(|| Arc::downgrade(player));
        true
    }

    pub(super) fn retire(&self) -> ReadyConnections {
        self.ready().take().unwrap_or_default()
    }

    fn take_ready(&self) -> ReadyConnections {
        self.ready()
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    pub(super) fn drain(
        &self,
        world: &World,
        server: &Server,
        extent: &RegionChunks,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) {
        let ready = self.take_ready();
        if ready.is_empty() {
            return;
        }
        let Some(_native) = server.plugin_manager.try_without_native_code() else {
            return;
        };
        for (id, player) in ready {
            if let Some(player) = player.upgrade()
                && let Some(client) = player.client.bedrock()
                && client.drain_regional_input(&player, server, extent, |position| {
                    player.can_simulate_in(world) && owns(position)
                })
            {
                // The owner can change after this job takes its ready list.
                player.world().offer_regional_input(id, &player);
            }
        }
    }
}

impl RegionLayout {
    pub(super) fn transfer_regional_inputs(&self, next: &Self) {
        // Existing jobs can still return ready connections after retirement. They
        // look up the current extent in the same way as a newly arrived packet.
        for retired in self.replaced_inputs(next) {
            for (id, player) in retired.retire() {
                if let Some(player) = player.upgrade() {
                    player.world().offer_regional_input(id, &player);
                }
            }
        }
    }
}

impl World {
    pub(crate) fn offer_regional_input(&self, id: u64, player: &Arc<Player>) {
        if !player.can_simulate_in(self) {
            return;
        }
        loop {
            let layout = self.region_layout.load();
            let Some(input) = layout
                .owner(player.get_entity().chunk_pos.load())
                .and_then(|owner| layout.input(owner))
            else {
                return;
            };
            if input.offer(id, player) {
                return;
            }
            // The publisher installed the new layout before closing this queue.
        }
    }
}
