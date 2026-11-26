use std::sync::{Arc, atomic::Ordering::Relaxed};

use pumpkin_data::{entity::EntityType, packet::CURRENT_MC_VERSION};
use rustc_hash::FxHashMap;
use sculith_protocol::java::client::play::CEntityVelocity;
use sculith_util::math::{position::BlockPos, vector2::Vector2, vector3::Vector3};

use crate::{
    entity::{Entity, EntityBase, drop_motion::ActorTickOwner},
    net::{ClientPlatform, java::JavaClient},
    server::{Server, TickDeadline},
    world::{RegionId, World},
};

use super::{ThrownItemEntity, egg::EggEntity, snowball::SnowballEntity, update_rotation};

const MAX_REGIONAL_SWEEP_SQUARED: f64 = 9.0;

pub fn supports_regional_flight(entity_type: &EntityType) -> bool {
    entity_type == &EntityType::SNOWBALL || entity_type == &EntityType::EGG
}

pub fn regional_thrown_item(actor: &dyn EntityBase) -> Option<&ThrownItemEntity> {
    if !supports_regional_flight(actor.get_entity().entity_type) {
        return None;
    }
    let data = actor as &dyn std::any::Any;
    data.downcast_ref::<SnowballEntity>()
        .map(|snowball| &snowball.thrown)
        .or_else(|| data.downcast_ref::<EggEntity>().map(|egg| &egg.thrown))
}

impl ThrownItemEntity {
    pub(crate) fn tick_from_world(
        &self,
        caller: &dyn EntityBase,
        server: &Server,
        deadline: Option<TickDeadline>,
    ) {
        let (_motion, advance) = self.entity.begin_world_motion(deadline);
        if advance && !self.entity.is_removed() {
            self.entity.tick_count.fetch_add(1, Relaxed);
            if self.entity.has_vehicle() {
                self.entity.velocity.store(Vector3::default());
            }
            self.process_tick(caller, server);
        }
    }

    pub(crate) fn try_regional_flight(
        &self,
        world: &World,
        chunk: Vector2<i32>,
        owner: RegionId,
        deadline: TickDeadline,
        owns: &impl Fn(Vector2<i32>) -> bool,
        publish: impl FnOnce(),
    ) {
        let entity = &self.entity;
        entity.with_regional_motion(
            world,
            chunk,
            owner,
            deadline,
            |clock| {
                // Launch events and clearing the shooter's vehicle group belong to the world tick.
                if entity.first_tick.load(Relaxed) || !self.owner.ready_for_regional_flight() {
                    return;
                }
                let velocity = self.flight_velocity(false);
                let distance = velocity.length_squared();
                if !(distance > 0.0 && distance <= MAX_REGIONAL_SWEEP_SQUARED) {
                    return;
                }
                let origin = entity.pos.load();
                let position = origin + velocity;
                if position == origin || !position.length_squared().is_finite() {
                    return;
                }
                let destination = BlockPos::floored_v(position);
                let next_chunk = destination.chunk_position();
                // Legacy Java viewers need their ordinary velocity translation.
                if world.players.load().iter().any(|player| {
                    matches!(player.client.as_ref(), ClientPlatform::Java(client)
                        if client.version.load() != CURRENT_MC_VERSION)
                        && player
                            .watched_section
                            .load()
                            .is_within_distance(next_chunk.x, next_chunk.y)
                }) {
                    return;
                }
                let bounds = entity.bounding_box.load();
                let next = Entity::make_bounding_box(
                    entity.entity_type,
                    position,
                    &entity.entity_dimension.load(),
                );
                // Include the same entity search margin as the ordinary projectile trace.
                let sweep = bounds.minmax(&next).expand_all(1.0);
                let min = sweep.min_block_pos();
                let max = sweep.max_block_pos();
                if !world.is_in_build_limit(min) || !world.is_in_build_limit(max) {
                    return;
                }
                let Ok(border) = world.worldborder.try_lock() else {
                    return;
                };
                if !border.contains(sweep.min.x, sweep.min.z)
                    || !border.contains(sweep.max.x, sweep.max.z)
                {
                    return;
                }
                world
                    .level
                    .try_with_clear_regional_path(min, max, destination, owns, |biome| {
                        world.entity_index.try_update_isolated_actor_bounds(
                            world,
                            entity.entity_id,
                            sweep,
                            sweep,
                            next,
                            || {
                                if !clock.claim(ActorTickOwner::Region(owner), deadline) {
                                    return false;
                                }
                                entity.tick_count.fetch_add(1, Relaxed);
                                entity.update_last_pos();
                                entity
                                    .was_in_powder_snow
                                    .store(entity.is_in_powder_snow.swap(false, Relaxed), Relaxed);
                                entity.water_height.store(0.0);
                                entity.lava_height.store(0.0);
                                entity.velocity.store(velocity);
                                entity.set_pos_with(
                                    position,
                                    || entity.bounding_box.value.store(next),
                                    |_| biome,
                                );
                                update_rotation(entity, false);
                                tracing::trace!(
                                    entity = entity.entity_id,
                                    region = owner,
                                    x = position.x,
                                    y = position.y,
                                    z = position.z,
                                    "Advanced thrown item on its region"
                                );
                                true
                            },
                        );
                    });
                drop(border);
            },
            publish,
        );
    }

    pub(crate) fn queue_regional_java_velocity(
        &self,
        world: &World,
        java_recipients: &mut FxHashMap<u64, Arc<ClientPlatform>>,
    ) {
        let entity = &self.entity;
        let chunk = entity.chunk_pos.load();
        let packet = CEntityVelocity::new(entity.entity_id.into(), entity.velocity.load());
        let mut encoded = None;
        for player in world.players.load().iter() {
            if let ClientPlatform::Java(client) = player.client.as_ref()
                && client.version.load() == CURRENT_MC_VERSION
                && player
                    .watched_section
                    .load()
                    .is_within_distance(chunk.x, chunk.y)
            {
                let data = encoded.get_or_insert_with(|| {
                    JavaClient::serialize_packet_for_version(&packet, CURRENT_MC_VERSION)
                });
                if data
                    .as_ref()
                    .is_ok_and(|data| client.try_enqueue_packet_data_unobserved(data.clone()))
                {
                    java_recipients
                        .entry(client.id)
                        .or_insert_with(|| player.client.clone());
                }
            }
        }
    }
}
