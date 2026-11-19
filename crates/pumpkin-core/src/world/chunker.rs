use pumpkin_util::math::{position::BlockPos, vector2::Vector2};
use std::{num::NonZero, sync::Arc};

use pumpkin_protocol::{
    bedrock::client::network_chunk_publisher_update::CNetworkChunkPublisherUpdate,
    java::client::play::CCenterChunk,
};
use pumpkin_world::cylindrical_chunk_iterator::Cylindrical;

use crate::{
    entity::{
        EntityBase,
        player::{HeldChunkTickets, Player},
    },
    net::ClientPlatform,
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
    config: &pumpkin_config::AdvancedConfiguration,
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

fn update_chunk_tickets(player: &Player, world: &World, view: Cylindrical, actual: Vector2<i32>) {
    use pumpkin_world::chunk_system::ChunkLoading;
    let mut held = player
        .held_chunk_tickets
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let generate = !player.is_spectator()
        || world
            .level_info
            .load()
            .game_rules
            .spectators_generate_chunks;
    let next = HeldChunkTickets {
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
    };
    if *held == next {
        return;
    }
    let mut loading = world
        .level
        .chunk_loading
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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

pub fn update_position(player: &Arc<Player>) {
    let entity = player.get_entity();
    let actual = entity.chunk_pos.load();
    let old_cylindrical = player.watched_section.load();

    // Vanilla `ChunkMap.move` -> re-pair on every move, not only on a view change.
    let world = player.world();
    world.entity_tracker.update_player_position(player, &world);
    let distance = get_view_distance(player);
    let pos = entity.block_pos.load();
    let new_cylindrical = match player.client.as_ref() {
        ClientPlatform::Java(_) => Cylindrical::new(actual, distance),
        ClientPlatform::Bedrock(_) => bedrock_view(old_cylindrical, pos, distance),
    };
    update_chunk_tickets(player, &world, new_cylindrical, actual);
    if old_cylindrical == new_cylindrical {
        return;
    }
    let new_chunk_center = new_cylindrical.center;
    match player.client.as_ref() {
        ClientPlatform::Java(java_client) => {
            java_client.try_send_packet(&CCenterChunk {
                chunk_x: new_chunk_center.x.into(),
                chunk_z: new_chunk_center.y.into(),
            });
        }
        ClientPlatform::Bedrock(bedrock_client) => {
            let origin = if new_cylindrical.view_distance == distance {
                pos
            } else {
                BlockPos::new(
                    new_chunk_center.x * 16 + 8,
                    pos.0.y,
                    new_chunk_center.y * 16 + 8,
                )
            };
            if let Ok(data) = bedrock_client.serialize_packet(&CNetworkChunkPublisherUpdate::new(
                origin,
                u32::from(new_cylindrical.view_distance.get()) * 16,
            )) {
                bedrock_client.try_enqueue_packet(data);
            }
        }
    }
    let (loading_iter, unloading_iter) =
        Cylindrical::changed_chunks(old_cylindrical, new_cylindrical);
    let loading_chunks: Vec<_> = loading_iter.collect();
    let unloading_chunks: Vec<_> = unloading_iter.collect();

    {
        let mut sender = player
            .chunk_sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for pos in &unloading_chunks {
            sender.unload_chunk(&player.client, *pos);
        }
        for pos in &loading_chunks {
            sender.enqueue_chunk(*pos);
        }
    }
    player.watched_section.store(new_cylindrical);

    // Make sure the watched section and the chunk watcher updates are async atomic. We want to
    // ensure what we unload when the player disconnects is correct.
    if !loading_chunks.is_empty() || !unloading_chunks.is_empty() {
        let level = world.level.clone();
        let world_clone = world.clone();
        let loading_chunks_clone = loading_chunks.clone();
        let unloading_chunks_clone = unloading_chunks;

        if let Some(server) = world.server.upgrade() {
            server.spawn_task(async move {
                level.mark_chunks_as_newly_watched(&loading_chunks_clone);
                let chunks_to_clean = level.mark_chunks_as_not_watched(&unloading_chunks_clone);

                if !chunks_to_clean.is_empty() {
                    world_clone
                        .remove_entities_in_chunks(&chunks_to_clean)
                        .await;
                    world_clone.level.clean_entity_chunks(&chunks_to_clean);
                }
            });
        }
    }

    if !loading_chunks.is_empty() {
        world.spawn_world_entity_chunks(player.clone(), loading_chunks);
    }
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
