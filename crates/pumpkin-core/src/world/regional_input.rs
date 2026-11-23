use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use pumpkin_util::math::{position::BlockPos, vector2::Vector2};
use rustc_hash::FxHashMap;

use crate::{
    entity::{Entity, EntityBase, player::Player},
    plugin::server::packet::{BedrockConnectionPacketSentEvent, PacketSentEvent},
    server::{Server, TickDeadline},
};

use super::{RegionChunks, RegionLayout, World};

bitflags::bitflags! {
    #[derive(Clone, Copy)]
    struct RegionalWork: u8 {
        const INPUT = 1;
        const EXPERIENCE_PICKUP = 2;
        const MINING = 4;
        const TERRAIN = 8;
        const ITEM_COOLDOWNS = 16;
        const INVENTORY_EDITS = 32;
        const FOOD_DRAIN = 64;
    }
}

type ReadyConnections = FxHashMap<u64, (Weak<Player>, RegionalWork)>;

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

    fn offer(&self, id: u64, player: &Arc<Player>, work: RegionalWork) -> bool {
        let mut queue = self.ready();
        let Some(ready) = queue.as_mut() else {
            return false;
        };
        ready
            .entry(id)
            .and_modify(|(_, waiting)| *waiting |= work)
            .or_insert_with(|| (Arc::downgrade(player), work));
        true
    }

    fn retire(&self) -> ReadyConnections {
        self.ready().take().unwrap_or_default()
    }

    fn remove(&self, id: u64) {
        if let Some(ready) = self.ready().as_mut() {
            ready.remove(&id);
        }
    }

    fn take_ready(&self) -> ReadyConnections {
        self.ready()
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    pub(super) fn drain(
        &self,
        world: &Arc<World>,
        server: &Server,
        extent: &RegionChunks,
        deadline: TickDeadline,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) {
        let ready = self.take_ready();
        if ready.is_empty() {
            return;
        }
        let Some(_native) = server.plugin_manager.try_without_native_code() else {
            return;
        };
        for (id, (player, work)) in ready {
            let Some(player) = player.upgrade() else {
                continue;
            };
            let Some(client) = player.client.bedrock() else {
                continue;
            };
            let Ok(_position) = client.regional_work().try_lock() else {
                player.world().offer_regional_work(id, &player, work);
                continue;
            };
            let owns = |position| player.can_simulate_in(world) && owns(position);
            let mut pending = RegionalWork::empty();
            if work.contains(RegionalWork::INPUT)
                && client.drain_regional_input(&player, server, extent, owns)
            {
                pending |= RegionalWork::INPUT;
            }
            if work.contains(RegionalWork::EXPERIENCE_PICKUP)
                && client.advance_regional_experience_pickup(&player, deadline, || {
                    owns(player.get_entity().chunk_pos.load())
                })
            {
                pending |= RegionalWork::EXPERIENCE_PICKUP;
            }
            if work.contains(RegionalWork::MINING)
                && client.advance_regional_mining(world, &player, deadline, owns)
            {
                pending |= RegionalWork::MINING;
            }
            let owns_player = || owns(player.get_entity().chunk_pos.load());
            if work.intersects(RegionalWork::INPUT | RegionalWork::FOOD_DRAIN)
                && player.hunger_manager.has_exhaustion_to_drain()
                && !server.tick_rate_manager.is_frozen()
                && !server.plugin_manager.has_handlers::<PacketSentEvent>()
                && !server.plugin_manager.has_handlers::<BedrockConnectionPacketSentEvent>()
                // Layout publication takes the writer. A busy layout keeps this work pending.
                && world.regional_drops.try_read().map_or(true, |_layout| {
                    player.hunger_manager.drain_on_region(&player, client, deadline, owns_player)
                })
            {
                pending |= RegionalWork::FOOD_DRAIN;
            }
            if work.contains(RegionalWork::ITEM_COOLDOWNS)
                && client.advance_regional_item_cooldowns(&player, deadline, owns_player)
            {
                pending |= RegionalWork::ITEM_COOLDOWNS;
            }
            if work.contains(RegionalWork::INVENTORY_EDITS)
                && client.inventory_edits().drain_region(
                    server,
                    world,
                    &player,
                    client,
                    owns_player,
                )
            {
                pending |= RegionalWork::INVENTORY_EDITS;
            }
            if work.contains(RegionalWork::TERRAIN)
                && player.can_dispatch_regional_terrain(client, server)
                && (!owns_player()
                    || player.tick_bedrock_chunk_delivery(
                        world,
                        client,
                        Some(deadline),
                        Some(&owns_player),
                    ))
            {
                pending |= RegionalWork::TERRAIN;
            }
            if !pending.is_empty() {
                // The owner can change after this job takes its ready list.
                player.world().offer_regional_work(id, &player, pending);
            }
        }
    }
}

impl RegionLayout {
    pub(super) fn transfer_regional_inputs(&self, next: &Self) {
        // Existing jobs can still return ready connections after retirement. They
        // look up the current extent in the same way as a newly arrived packet.
        for retired in self.replaced_inputs(next) {
            for (id, (player, work)) in retired.retire() {
                if let Some(player) = player.upgrade() {
                    player.world().offer_regional_work(id, &player, work);
                }
            }
        }
    }
}

impl World {
    pub(crate) fn move_player_across_chunks(&self, entity: &Entity, commit: impl FnOnce()) {
        let Some(player) = self
            .get_player_by_uuid(entity.entity_uuid)
            .filter(|player| {
                std::ptr::eq(player.get_entity(), entity) && player.can_simulate_in(self)
            })
        else {
            commit();
            return;
        };
        let Some(client) = player
            .client
            .bedrock()
            .filter(|client| client.is_current_protocol())
        else {
            commit();
            return;
        };

        // Finish this connection's claimed regional work before its owner changes.
        let _position = client
            .regional_work()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let from = entity.chunk_pos.load();
        commit();
        let to = entity.chunk_pos.load();
        let layout = self.region_layout.load();
        let previous = layout.owner(from);
        let destination = layout.owner(to);
        if previous == destination || destination.and_then(|owner| layout.input(owner)).is_none() {
            return;
        }
        if let Some(input) = previous.and_then(|owner| layout.input(owner)) {
            input.remove(client.id);
        }
        // A dispatcher can already have taken the old map. The FIFO and timer
        // state still belong to the player, so wake the destination directly.
        if self.offer_regional_work(client.id, &player, RegionalWork::all()) {
            tracing::trace!(player = %player.gameprofile.name, ?from, ?to, "Moved Bedrock regional work with the player");
        }
    }

    pub(crate) fn offer_regional_input(&self, id: u64, player: &Arc<Player>) {
        self.offer_regional_work(id, player, RegionalWork::INPUT);
    }

    pub(crate) fn offer_regional_experience_pickup(&self, id: u64, player: &Arc<Player>) -> bool {
        self.offer_regional_work(id, player, RegionalWork::EXPERIENCE_PICKUP)
    }

    pub(crate) fn offer_regional_terrain(&self, id: u64, player: &Arc<Player>) -> bool {
        self.offer_regional_work(id, player, RegionalWork::TERRAIN)
    }

    pub(crate) fn offer_regional_item_cooldowns(&self, id: u64, player: &Arc<Player>) -> bool {
        self.offer_regional_work(id, player, RegionalWork::ITEM_COOLDOWNS)
    }

    pub(crate) fn offer_regional_food_drain(&self, id: u64, player: &Arc<Player>) {
        self.offer_regional_work(id, player, RegionalWork::FOOD_DRAIN);
    }

    pub(crate) fn offer_regional_inventory_edits(&self, id: u64, player: &Arc<Player>) {
        self.offer_regional_work(id, player, RegionalWork::INVENTORY_EDITS);
    }

    pub(crate) fn offer_regional_mining(
        &self,
        id: u64,
        player: &Arc<Player>,
        target: BlockPos,
    ) -> bool {
        let layout = self.region_layout.load();
        let target_owner = layout.owner(target.chunk_position());
        target_owner.is_some()
            && target_owner == layout.owner(player.get_entity().chunk_pos.load())
            && self.offer_regional_work(id, player, RegionalWork::MINING)
    }

    fn offer_regional_work(&self, id: u64, player: &Arc<Player>, work: RegionalWork) -> bool {
        if !player.can_simulate_in(self) {
            return false;
        }
        loop {
            let layout = self.region_layout.load();
            let Some(input) = layout
                .owner(player.get_entity().chunk_pos.load())
                .and_then(|owner| layout.input(owner))
            else {
                return false;
            };
            if input.offer(id, player, work) {
                return true;
            }
            // The publisher installed the new layout before closing this queue.
        }
    }
}
