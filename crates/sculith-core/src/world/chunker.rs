use sculith_util::math::{position::BlockPos, vector2::Vector2};
use std::{num::NonZero, sync::Arc};

use sculith_protocol::{
    bedrock::client::network_chunk_publisher_update::CNetworkChunkPublisherUpdate,
    java::client::play::CCenterChunk, packet::Packet, serial::PacketWrite,
};
use sculith_world::chunk_system::ChunkLoading;
use sculith_world::cylindrical_chunk_iterator::Cylindrical;

use crate::{
    entity::{
        EntityBase,
        player::{HeldChunkTickets, Player},
    },
    net::{ClientPlatform, bedrock::BedrockClient},
    world::World,
};

pub fn get_view_distance(player: &Player) -> NonZero<u8> {
    let fallback = NonZero::new(2).unwrap_or(NonZero::<u8>::MIN);
    let Some(server) = player.world().server.upgrade() else {
        return fallback;
    };
    let max_view_distance = match player.client.as_ref() {
        ClientPlatform::Java(_) => server.advanced_config.networking.java.view_distance,
        ClientPlatform::Bedrock(_) => server.advanced_config.networking.bedrock.view_distance,
    };
    player
        .config
        .load()
        .view_distance
        .clamp(fallback, max_view_distance)
}

pub(super) fn simulation_distance(
    player: &Player,
    config: &sculith_config::AdvancedConfiguration,
) -> u8 {
    match player.client.as_ref() {
        ClientPlatform::Java(_) => config.networking.java.simulation_distance.get(),
        ClientPlatform::Bedrock(_) => config.networking.bedrock.simulation_distance.get(),
    }
}

// Checks if the target chunk is within Chebyshev distance (L_infinity) of the center chunk.
#[must_use]
#[inline]
pub fn is_within_chebyshev_distance(
    center: Vector2<i32>,
    target: Vector2<i32>,
    distance: i32,
) -> bool {
    (target.x - center.x).abs().max((target.y - center.y).abs()) <= distance
}

// Crossing a chunk boundary by one block must not evict the far edge of a Bedrock view.
// Two extra chunks cover a one-chunk displacement on both axes.
fn bedrock_view(old: Cylindrical, pos: BlockPos, distance: NonZero<u8>) -> Cylindrical {
    let actual = pos.chunk_position();
    let radius = distance.get();
    if !(4..=pumpkin_data::chunk_view_lut::MAX_VIEW_DISTANCE - 2).contains(&radius) {
        return Cylindrical::new(actual, distance);
    }
    let retained_radius = distance.saturating_add(2);
    let center = old.center;
    if old.view_distance == retained_radius
        && (i64::from(pos.0.x) - (i64::from(center.x) * 16 + 8)).abs() <= 16
        && (i64::from(pos.0.z) - (i64::from(center.y) * 16 + 8)).abs() <= 16
    {
        return old;
    }
    Cylindrical::new(actual, retained_radius)
}

pub(crate) fn bedrock_view_is_current(player: &Player, published: Cylindrical) -> bool {
    let pos = player.get_entity().block_pos.load();
    bedrock_view(published, pos, get_view_distance(player)) == published
}

fn desired_chunk_tickets(
    player: &Player,
    world: &World,
    view: Cylindrical,
    actual: Vector2<i32>,
) -> HeldChunkTickets {
    let generate = !player.is_spectator()
        || world
            .level_info
            .load()
            .game_rules
            .spectators_generate_chunks;
    HeldChunkTickets {
        view: generate.then(|| {
            (
                view.center,
                ChunkLoading::get_level_from_view_distance(
                    view.view_distance.get().saturating_add(1),
                ),
            )
        }),
        simulation: generate.then(|| {
            let distance = world
                .server
                .upgrade()
                .map_or(10, |s| simulation_distance(player, &s.advanced_config));
            (
                actual,
                ChunkLoading::get_level_from_simulation_distance(distance),
            )
        }),
    }
}

fn update_chunk_tickets(player: &Player, world: &World, view: Cylindrical, actual: Vector2<i32>) {
    let mut held = player
        .held_chunk_tickets
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let next = desired_chunk_tickets(player, world, view, actual);
    if *held == next {
        return;
    }
    let mut loading = world
        .level
        .chunk_loading
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    replace_chunk_tickets(&mut loading, &mut held, next);
}

fn replace_chunk_tickets(
    loading: &mut ChunkLoading,
    held: &mut HeldChunkTickets,
    next: HeldChunkTickets,
) {
    for (before, after) in [(held.view, next.view), (held.simulation, next.simulation)] {
        if before != after {
            if let Some((pos, level)) = after {
                loading.add_ticket(pos, level);
            }
            if let Some((pos, level)) = before {
                loading.remove_ticket(pos, level);
            }
        }
    }
    *held = next;
    loading.send_change();
}

pub(crate) struct EntityChunkWatchChanges {
    load: Vec<Vector2<i32>>,
    retire: Vec<Vector2<i32>>,
}

impl EntityChunkWatchChanges {
    fn register(world: &World, loading: &[Vector2<i32>], unloading: &[Vector2<i32>]) -> Self {
        Self {
            load: world.level.mark_chunks_as_newly_watched(loading),
            retire: world.level.mark_chunks_as_not_watched(unloading),
        }
    }

    fn dispatch(self, world: &Arc<World>) {
        if !self.retire.is_empty() {
            world.queue_entity_chunk_changes(self.retire);
        }
        if !self.load.is_empty() {
            world.request_entity_chunks(self.load);
        }
    }
}

pub(crate) enum RegionalBedrockViewUpdate {
    Retained,
    Recentered(EntityChunkWatchChanges),
}

impl RegionalBedrockViewUpdate {
    pub(crate) fn dispatch(self, world: &Arc<World>, player: &Player) {
        if let Self::Recentered(changes) = self {
            changes.dispatch(world);
            if let Some(client) = player.client.bedrock()
                && let Some(registered) = client.player.load().as_ref().as_ref()
                && std::ptr::eq(registered.as_ref(), player)
            {
                world.offer_regional_terrain(client.id, registered);
            }
        }
    }
}

/// Advances the interest of a grounded Bedrock player moving within its owner.
pub(crate) fn try_advance_bedrock_view(
    player: &Player,
    world: &World,
    from: BlockPos,
) -> Option<RegionalBedrockViewUpdate> {
    let mut registered = player.registered_chunk_view.try_lock().ok()?;
    let mut held = player.held_chunk_tickets.try_lock().ok()?;
    let old = (*registered)?;
    if player.watched_section.load() != old {
        return None;
    }
    let pos = player.get_entity().block_pos.load();
    let actual = pos.chunk_position();
    let distance = get_view_distance(player);
    let view = bedrock_view(old, pos, distance);
    let next = desired_chunk_tickets(player, world, view, actual);
    if old == view && *held == next {
        world.entity_tracker.try_mark_player_position(player)?;
        return Some(RegionalBedrockViewUpdate::Retained);
    }
    let client = player.client.bedrock()?;
    if !client.chunk_view_published()
        || *held != desired_chunk_tickets(player, world, old, from.chunk_position())
        || next.simulation.is_none()
        || !old.is_within_distance(actual.x, actual.y)
        || (old != view
            && (!player.has_stable_world_membership()
                || bedrock_view(old, from, distance) != old
                || !is_within_chebyshev_distance(old.center, view.center, 1)))
    {
        return None;
    }
    let mut sender = player.chunk_sender.try_lock().ok()?;
    if !sender.is_chunk_ready(&actual) {
        return None;
    }
    let mut loading = world.level.chunk_loading.try_lock().ok()?;
    world.entity_tracker.try_mark_player_position(player)?;
    if old == view {
        replace_chunk_tickets(&mut loading, &mut held, next);
        return Some(RegionalBedrockViewUpdate::Retained);
    }

    let mut publisher = Vec::new();
    bedrock_view_packet(view, pos, distance)
        .write(&mut publisher)
        .ok()?;
    // Terrain and LOD commit under the sender lock. Keep the old view intact
    // until the queue accepts the publisher, then install its frontier.
    if !client.is_current_protocol()
        || client.observes_play_packets()
        || !client.try_enqueue_raw_packet_direct(
            CNetworkChunkPublisherUpdate::PACKET_ID as u16,
            &publisher,
        )
    {
        return None;
    }
    let revision = client.begin_chunk_view();
    let (entered, departed) = Cylindrical::changed_chunks(old, view);
    let entered: Vec<_> = entered.collect();
    let departed: Vec<_> = departed.collect();
    let changes = EntityChunkWatchChanges::register(world, &entered, &departed);
    for pos in departed {
        sender.unload_chunk(&player.client, pos);
    }
    for pos in entered {
        sender.enqueue_chunk(pos);
    }
    replace_chunk_tickets(&mut loading, &mut held, next);
    *registered = Some(view);
    player.watched_section.store(view);
    client.publish_chunk_view(revision);
    Some(RegionalBedrockViewUpdate::Recentered(changes))
}

pub fn update_position(player: &Arc<Player>) {
    let world = player.world();
    let entity = player.get_entity();
    // Observe the latest view before applying a delta. Departure uses this
    // same guard, so it cannot release a view that has not been registered.
    let mut registered = player
        .registered_chunk_view
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if player.client.closed()
        || !Arc::ptr_eq(&world, &player.world())
        || (registered.is_none()
            && !world
                .players
                .load()
                .iter()
                .any(|active| Arc::ptr_eq(active, player)))
    {
        return;
    }
    let actual = entity.chunk_pos.load();
    let distance = get_view_distance(player);
    let pos = entity.block_pos.load();
    let old_cylindrical = player.watched_section.load();
    let new_cylindrical = match player.client.as_ref() {
        ClientPlatform::Java(_) => Cylindrical::new(actual, distance),
        ClientPlatform::Bedrock(_) => bedrock_view(old_cylindrical, pos, distance),
    };
    update_chunk_tickets(player, &world, new_cylindrical, actual);
    // Vanilla `ChunkMap.move` re-pairs on every move, including within one view.
    world.entity_tracker.update_player_position(player, &world);
    if *registered == Some(new_cylindrical) {
        return;
    }
    let publication = player
        .client
        .bedrock()
        .map(|client| (client, client.begin_chunk_view()));
    let (loading_chunks, unloading_chunks) = registered.map_or_else(
        || (new_cylindrical.all_chunks_within().collect(), Vec::new()),
        |old| {
            let (loading, unloading) = Cylindrical::changed_chunks(old, new_cylindrical);
            (loading.collect::<Vec<_>>(), unloading.collect::<Vec<_>>())
        },
    );
    let changes = EntityChunkWatchChanges::register(&world, &loading_chunks, &unloading_chunks);
    // A Bedrock delivery commits under this lock. Retire the old part of the view
    // before queueing the publisher packet, whose plugin callbacks run outside it.
    if publication.is_some() && !unloading_chunks.is_empty() {
        let mut sender = player
            .chunk_sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for &pos in &unloading_chunks {
            sender.unload_chunk(&player.client, pos);
        }
    }
    *registered = Some(new_cylindrical);
    player.watched_section.store(new_cylindrical);
    drop(registered);

    let new_chunk_center = new_cylindrical.center;
    let publisher_queued = match player.client.as_ref() {
        ClientPlatform::Java(java_client) => {
            java_client.try_send_packet(&CCenterChunk {
                chunk_x: new_chunk_center.x.into(),
                chunk_z: new_chunk_center.y.into(),
            });
            false
        }
        ClientPlatform::Bedrock(client) => {
            queue_bedrock_view(client, new_cylindrical, pos, distance)
        }
    };
    {
        let mut sender = player
            .chunk_sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if publication.is_none() {
            for pos in unloading_chunks {
                sender.unload_chunk(&player.client, pos);
            }
        }
        for pos in &loading_chunks {
            sender.enqueue_chunk(*pos);
        }
        if publisher_queued && let Some((client, revision)) = publication {
            client.publish_chunk_view(revision);
        }
    }
    changes.dispatch(&world);
}

fn queue_bedrock_view(
    client: &BedrockClient,
    view: Cylindrical,
    pos: BlockPos,
    distance: NonZero<u8>,
) -> bool {
    client
        .serialize_packet(&bedrock_view_packet(view, pos, distance))
        .is_ok_and(|data| client.try_enqueue_packet_data(data))
}

fn bedrock_view_packet(
    view: Cylindrical,
    pos: BlockPos,
    distance: NonZero<u8>,
) -> CNetworkChunkPublisherUpdate {
    let origin = if view.view_distance == distance {
        pos
    } else {
        BlockPos::new(view.center.x * 16 + 8, pos.0.y, view.center.y * 16 + 8)
    };
    CNetworkChunkPublisherUpdate::new(origin, u32::from(view.view_distance.get()) * 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bedrock_boundary_movement_keeps_the_entire_requested_view() {
        let empty = Cylindrical::new(Vector2::new(0, 0), NonZero::<u8>::MIN);
        for radius in 4..=30 {
            let requested = NonZero::new(radius).unwrap();
            let anchored = bedrock_view(empty, BlockPos::new(0, 70, 0), requested);
            assert_eq!(anchored.view_distance, requested.saturating_add(2));
            for x in [-8, 0, 8, 16, 24] {
                for z in [-8, 0, 8, 16, 24] {
                    let pos = BlockPos::new(x, 70, z);
                    let window = bedrock_view(anchored, pos, requested);
                    assert_eq!(anchored.center, window.center);
                    let actual = Cylindrical::new(pos.chunk_position(), requested);
                    assert!(
                        actual
                            .all_chunks_within()
                            .all(|p| window.is_within_distance(p.x, p.y))
                    );
                }
            }
        }
    }

    #[test]
    fn bedrock_terrain_recenters_on_travel_teleport_or_radius_change() {
        let radius = NonZero::<u8>::new(16).unwrap();
        let old = Cylindrical::new(Vector2::new(0, 0), radius.saturating_add(2));
        let moved = bedrock_view(old, BlockPos::new(25, 70, 8), radius);
        assert_eq!(moved.center, Vector2::new(1, 0));
        assert_eq!(
            bedrock_view(moved, BlockPos::new(23, 70, 8), radius).center,
            moved.center
        );
        let far = BlockPos::new(-1025, 80, 1025);
        assert_eq!(
            bedrock_view(moved, far, radius).center,
            far.chunk_position()
        );
        for r in [2, 3, 31, 32] {
            let requested = NonZero::new(r).unwrap();
            let window = bedrock_view(old, far, requested);
            assert_eq!(window.center, far.chunk_position());
            assert_eq!(window.view_distance, requested);
        }
    }
}
