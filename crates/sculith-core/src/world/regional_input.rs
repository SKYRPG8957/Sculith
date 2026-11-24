use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak, atomic::Ordering::Relaxed};

use pumpkin_data::game_event::GameEvent;
use rustc_hash::FxHashMap;
use sculith_util::math::{position::BlockPos, vector2::Vector2};

use crate::{
    entity::{
        Entity, EntityBase, breath::BreathManager, living::RegionalFoodUseFinish, player::Player,
    },
    net::{ClientPlatform, bedrock::BedrockClient},
    plugin::api::events::entity::entity_air_change::EntityAirChangeEvent,
    plugin::api::events::world::generic_game::GenericGameEvent,
    plugin::server::packet::{BedrockConnectionPacketSentEvent, PacketSentEvent},
    server::{Server, TickDeadline},
};

use super::{RegionChunks, RegionLayout, World, entity_tracker::EntityTracker};

bitflags::bitflags! {
    #[derive(Clone, Copy)]
    struct RegionalWork: u16 {
        const INPUT = 1;
        const EXPERIENCE_PICKUP = 2;
        const MINING = 4;
        const TERRAIN = 8;
        const ITEM_COOLDOWNS = 16;
        const INVENTORY_EDITS = 32;
        const FOOD_DRAIN = 64;
        const STATUS_EFFECTS = 128;
        const BREATH = 256;
        const FOOD_USE = 512;
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

    pub(super) fn is_retired(&self) -> bool {
        self.ready().is_none()
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

    #[expect(
        clippy::too_many_lines,
        reason = "Keep the dispatch order of a player's regional work together"
    )]
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
        let mut java_recipients = FxHashMap::default();
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
            if work.contains(RegionalWork::ITEM_COOLDOWNS)
                && client.advance_regional_item_cooldowns(&player, deadline, owns_player)
            {
                pending |= RegionalWork::ITEM_COOLDOWNS;
            }
            if work.contains(RegionalWork::FOOD_USE)
                && world.advance_regional_food_use(
                    server,
                    &player,
                    client,
                    deadline,
                    owns_player,
                    &mut java_recipients,
                )
            {
                pending |= RegionalWork::FOOD_USE;
            }
            // A meal is committed before the same player's exhaustion is spent.
            if work.intersects(RegionalWork::INPUT | RegionalWork::FOOD_DRAIN)
                && world.drain_regional_player_food(server, &player, client, deadline, owns_player)
            {
                pending |= RegionalWork::FOOD_DRAIN;
            }
            if work.contains(RegionalWork::STATUS_EFFECTS)
                && world.advance_regional_player_effects(
                    server,
                    &player,
                    deadline,
                    owns_player,
                    &mut java_recipients,
                )
            {
                pending |= RegionalWork::STATUS_EFFECTS;
            }
            if work.contains(RegionalWork::BREATH)
                && world.advance_regional_player_breath(
                    server,
                    &player,
                    client,
                    deadline,
                    owns,
                    &mut java_recipients,
                )
            {
                pending |= RegionalWork::BREATH;
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
        EntityTracker::flush_regional_java_packets(java_recipients);
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
    fn drain_regional_player_food(
        &self,
        server: &Server,
        player: &Player,
        client: &BedrockClient,
        deadline: TickDeadline,
        owns: impl Fn() -> bool,
    ) -> bool {
        player.hunger_manager.has_exhaustion_to_drain()
            && !server.tick_rate_manager.is_frozen()
            && !server.plugin_manager.has_handlers::<PacketSentEvent>()
            && !server.plugin_manager.has_handlers::<BedrockConnectionPacketSentEvent>()
            // Layout publication takes the writer. A busy layout keeps this work pending.
            && self.regional_actors.try_read().map_or(true, |_layout| {
                player.hunger_manager.drain_on_region(player, client, deadline, owns)
            })
    }

    fn advance_regional_food_use(
        &self,
        server: &Server,
        player: &Player,
        client: &BedrockClient,
        deadline: TickDeadline,
        owns: impl FnOnce() -> bool,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
    ) -> bool {
        if client.food_use_slot().is_none()
            || !client.is_current_protocol()
            || !player.has_client_loaded()
            || !player.has_stable_world_membership()
            || server.tick_rate_manager.is_frozen()
        {
            return false;
        }
        let Ok(_layout) = self.regional_actors.try_read() else {
            return true;
        };
        if player.living_entity.item_use_time.load(Relaxed) == 1 {
            return self.finish_regional_food_use(
                server,
                player,
                client,
                deadline,
                owns,
                java_recipients,
            );
        }
        client
            .with_idle_play_ingress(player, || {
                player
                    .living_entity
                    .advance_regional_food_use(player, client, deadline, || {
                        owns() && player.has_stable_world_membership()
                    })
            })
            .unwrap_or(true)
    }

    fn finish_regional_food_use(
        &self,
        server: &Server,
        player: &Player,
        client: &BedrockClient,
        deadline: TickDeadline,
        owns: impl FnOnce() -> bool,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
    ) -> bool {
        let plugins = &server.plugin_manager;
        if plugins.has_handlers::<GenericGameEvent>()
            || plugins.has_handlers::<PacketSentEvent>()
            || plugins.has_handlers::<BedrockConnectionPacketSentEvent>()
        {
            return false;
        }
        let Some(_checkpoint) = server.try_entity_save_guard() else {
            return true;
        };

        let mut pending = true;
        client.with_idle_player_inventory(player, || {
            let result = player.living_entity.finish_food_use_on_region(
                player,
                client,
                deadline,
                |position| {
                    if !player.has_stable_world_membership() {
                        return Some(false);
                    }
                    if !owns() {
                        return None;
                    }
                    let chunk = BlockPos::floored_v(position).chunk_position();
                    if self.players.load().iter().any(|nearby| {
                        nearby
                            .client
                            .bedrock()
                            .is_some_and(|client| !client.is_current_protocol())
                            && super::is_within_chebyshev_distance(
                                chunk,
                                nearby.get_entity().chunk_pos.load(),
                                1,
                            )
                    }) {
                        return Some(false);
                    }
                    let area = super::game_event::notified_area(
                        position,
                        GameEvent::ItemInteractFinish.notification_radius(),
                    );
                    self.entity_index
                        .try_has_game_event_listener(&self.entities, &area)
                        .map(|has_listener| !has_listener)
                },
                |metadata, java, empty_main_hand| {
                    self.entity_tracker.try_publish_regional_player_metadata(
                        self,
                        player,
                        metadata,
                        java,
                        empty_main_hand,
                        java_recipients,
                    )
                },
            );
            if let RegionalFoodUseFinish::Finished(position) = result {
                self.send_bedrock_level_sound("burp", &position, -1, false);
            }
            pending = matches!(result, RegionalFoodUseFinish::Pending);
        });
        pending
    }

    fn advance_regional_player_breath(
        &self,
        server: &Server,
        player: &Player,
        client: &BedrockClient,
        deadline: TickDeadline,
        owns: impl Fn(Vector2<i32>) -> bool,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
    ) -> bool {
        if !player.has_client_loaded()
            || !player.has_stable_world_membership()
            || server.tick_rate_manager.is_frozen()
            || server.plugin_manager.has_handlers::<EntityAirChangeEvent>()
            || server.plugin_manager.has_handlers::<PacketSentEvent>()
            || server
                .plugin_manager
                .has_handlers::<BedrockConnectionPacketSentEvent>()
        {
            return false;
        }
        let Ok(_layout) = self.regional_actors.try_read() else {
            return true;
        };
        let Some(_checkpoint) = server.try_entity_save_guard() else {
            return true;
        };
        // An earlier movement packet might still take the player underwater.
        client
            .with_idle_play_ingress(player, || {
                player.breath_manager.advance_on_region(
                    self,
                    player,
                    deadline,
                    |chunk| owns(chunk) && player.has_stable_world_membership(),
                    |air, java| {
                        self.entity_tracker.try_publish_regional_player_metadata(
                            self,
                            player,
                            BreathManager::bedrock_air_metadata(air),
                            java,
                            false,
                            java_recipients,
                        )
                    },
                )
            })
            .unwrap_or(true)
    }

    fn advance_regional_player_effects(
        &self,
        server: &Server,
        player: &Player,
        deadline: TickDeadline,
        owns_player: impl Fn() -> bool,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
    ) -> bool {
        if !player
            .client
            .bedrock()
            .is_some_and(|client| client.is_current_protocol())
            || !player.has_client_loaded()
            || !player.has_stable_world_membership()
            || server.tick_rate_manager.is_frozen()
        {
            return false;
        }
        let Ok(_layout) = self.regional_actors.try_read() else {
            return true;
        };
        let Some(_checkpoint) = server.try_entity_save_guard() else {
            return true;
        };
        let owns_effects = || owns_player() && player.has_stable_world_membership();
        player
            .living_entity
            .advance_regional_effects(deadline, owns_effects, |expired, data| {
                if !owns_effects() {
                    return None;
                }
                self.entity_tracker.try_publish_regional_effect_changes(
                    self,
                    server,
                    player,
                    expired,
                    data,
                    java_recipients,
                )
            })
    }

    pub(super) fn resume_regional_player_work(
        &self,
        previous: &RegionLayout,
        current: &RegionLayout,
    ) {
        for player in self.players.load().iter() {
            let chunk = player.get_entity().chunk_pos.load();
            if previous.active_owner(chunk).is_none()
                && current.active_owner(chunk).is_some()
                && let Some(client) = player.client.bedrock()
                && client.is_current_protocol()
            {
                // A move into a chunk without an owner leaves work on the
                // connection. Wake it when the new extent becomes active.
                self.offer_regional_work(client.id, player, RegionalWork::all());
            }
        }
    }

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
        let previous = layout.active_owner(from);
        let destination = layout.active_owner(to);
        if previous == destination {
            return;
        }
        if let Some(input) = previous.and_then(|owner| layout.input(owner)) {
            input.remove(client.id);
        }
        // A dispatcher can already have taken the old map. The FIFO and timer
        // state still belong to the player, so wake the destination directly.
        if destination.is_some()
            && self.offer_regional_work(client.id, &player, RegionalWork::all())
        {
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

    pub(crate) fn offer_regional_status_effects(&self, id: u64, player: &Arc<Player>) {
        self.offer_regional_work(id, player, RegionalWork::STATUS_EFFECTS);
    }

    pub(crate) fn offer_regional_breath(&self, id: u64, player: &Arc<Player>) {
        self.offer_regional_work(id, player, RegionalWork::BREATH);
    }

    pub(crate) fn offer_regional_food_use(&self, id: u64, player: &Arc<Player>) {
        self.offer_regional_work(id, player, RegionalWork::FOOD_USE);
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
                .active_owner(player.get_entity().chunk_pos.load())
                .and_then(|owner| layout.input(owner))
            else {
                return false;
            };
            if input.offer(id, player, work) {
                return true;
            }
            // The publisher installed the new layout before closing this queue.
            if Arc::ptr_eq(&layout, &self.region_layout.load()) {
                return false;
            }
        }
    }
}
