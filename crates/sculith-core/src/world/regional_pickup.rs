//! Complete a same-region item pickup using the ordinary inventory plan.

use std::sync::{Arc, atomic::Ordering::Relaxed};

use pumpkin_data::{Block, statistic::StatisticCategory};
use pumpkin_inventory::Inventory;
use rustc_hash::FxHashMap;
use sculith_util::math::{position::BlockPos, vector2::Vector2};

use crate::{
    entity::{EntityBase, item::ItemEntity, player::Player},
    net::ClientPlatform,
    plugin::{
        api::events::{entity::EntityPickupItemEvent, player::PlayerStatisticIncrementEvent},
        server::packet::{BedrockConnectionPacketSentEvent, PacketSentEvent},
    },
    server::Server,
};

use super::{World, regional_campfires::RegionalEntityUpdate};

impl World {
    pub(super) fn regional_pickups_unobserved(server: &Server) -> bool {
        let plugins = &server.plugin_manager;
        !server.stop_token.is_cancelled()
            && !server.tick_rate_manager.is_frozen()
            && !plugins.has_handlers::<EntityPickupItemEvent>()
            && !plugins.has_handlers::<PlayerStatisticIncrementEvent>()
            && !plugins.has_handlers::<PacketSentEvent>()
            && !plugins.has_handlers::<BedrockConnectionPacketSentEvent>()
    }

    /// The actor step pins the drop's assignment and the live entity snapshot.
    pub(super) fn try_collect_regional_drop(
        &self,
        server: &Server,
        actor: &Arc<dyn EntityBase>,
        item: &ItemEntity,
        owns: &impl Fn(Vector2<i32>) -> bool,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
    ) -> Option<()> {
        if !item.can_be_picked_up_on_region() {
            return None;
        }
        let source = item.get_entity();
        let bounds = source.bounding_box.load();
        let contact = bounds.expand(1.0, 0.5, 1.0);
        let chunk = source.chunk_pos.load();
        let min = BlockPos::floored_v(contact.min.add_raw(0.0, -2.0, 0.0));
        let max = contact.max_block_pos();
        let drop_floor = BlockPos::floored_v(bounds.min.add_raw(0.0, -2.0, 0.0));
        let drop_top = bounds.max_block_pos();

        self.entity_index.try_with_pickup_contact(
            self,
            source.entity_id,
            &contact,
            |player, registered| {
                let client = player
                    .client
                    .bedrock()
                    .filter(|client| client.is_current_protocol())?;
                let _checkpoint = server.try_entity_save_guard()?;
                let _position = client.regional_work().try_lock().ok()?;
                let collector = player.get_entity();
                let vehicle = collector.vehicle.try_lock().ok()?;
                if vehicle.is_some() {
                    return None;
                }
                let _inventory = player.try_regional_inventory_edit()?;

                // Index membership stays pinned by the contact query. Also pin the
                // exact collector pose so even a same-cell movement cannot race the transfer.
                collector.bounding_box.try_with_current_world(self, || {
                    let local = || {
                        let current = source.bounding_box.load();
                        owns(chunk)
                            && source.chunk_pos.load() == chunk
                            && collector.chunk_pos.load() == chunk
                            && current.min == bounds.min
                            && current.max == bounds.max
                            && contact.intersects(&collector.bounding_box.load())
                            && std::ptr::eq(source.world.load().as_ref(), self)
                            && player.can_simulate_in(self)
                            && player.has_stable_world_membership()
                            && !player.living_entity.is_dead_or_dying()
                            && !player.is_spectator()
                            && item.can_be_picked_up_on_region()
                            && registered()
                    };
                    source.try_with_unmounted_actor(|| {
                        if !local() {
                            return None;
                        }
                        self.level.try_with_regional_path(
                            min,
                            max,
                            owns,
                            |read| {
                                // A hopper can pull between 0.6875 and 2 blocks above its base.
                                for position in BlockPos::iterate(drop_floor, drop_top) {
                                    if read(position)?.to_block() == &Block::HOPPER {
                                        return None;
                                    }
                                }
                                Some((source.block_pos.load(), ()))
                            },
                            |(), _| {
                                self.commit_regional_pickup(
                                    server,
                                    player,
                                    actor,
                                    item,
                                    local,
                                    java_recipients,
                                )
                            },
                        )?
                    })?
                })?
            },
        )
    }

    fn commit_regional_pickup(
        &self,
        server: &Server,
        player: &Arc<Player>,
        actor: &Arc<dyn EntityBase>,
        item: &ItemEntity,
        local: impl FnOnce() -> bool,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
    ) -> Option<()> {
        let client = player.client.bedrock()?;
        client.with_idle_player_inventory(player, |handler| {
            let mut updates = self.regional_entity_updates.0.try_lock().ok()?;
            let mut stack = item.get_item_stack().try_lock().ok()?;
            let pickup = player.inventory.plan_pickup(&stack)?;
            let count = pickup.count();
            let item_type = pickup.item();
            let (slot, after) = pickup.single_storage_update()?;
            let inventory: Arc<dyn Inventory> = player.inventory.clone();
            let screen_slot = handler.get_slot_index(&inventory, slot)?;
            let packet = client.encode_player_inventory_slot(slot, after)?;
            let after = after.clone();
            let mut stats = player.stats.try_lock().ok()?;
            let source = item.get_entity();
            self.entity_tracker.try_publish_regional_item_pickup(
                self,
                source,
                player,
                count,
                java_recipients,
                || {
                    let mut checkpoint = None;
                    if !player.inventory.commit_pickup(pickup, &mut stack, || {
                        if !local() {
                            return false;
                        }
                        let Ok(permit) = server.prepare_item_pickup(player, item, count, None)
                        else {
                            return false;
                        };
                        checkpoint = permit;
                        source.mark_discarded()
                    }) {
                        return None;
                    }
                    stats.increment(
                        StatisticCategory::PickedUp,
                        i32::from(item_type.id),
                        i32::from(count),
                    );
                    if let Some(checkpoint) = checkpoint {
                        checkpoint.record(item);
                    }
                    Some(())
                },
            )?;
            player.inventory_changed.store(true, Relaxed);
            if client.try_enqueue_packet_data_unobserved(packet) {
                handler.set_received_stack(screen_slot, after);
            }
            updates.push(RegionalEntityUpdate::Removed(actor.clone()));
            tracing::trace!(
                entity = source.entity_id,
                collector = player.entity_id(),
                count,
                "Collected a dropped stack on its region"
            );
            Some(())
        })?
    }
}
