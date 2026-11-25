use core::f32;
use std::sync::{
    Arc,
    atomic::{AtomicI32, AtomicU32, Ordering},
};

use pumpkin_data::{damage::DamageType, entity::EntityType};
use pumpkin_nbt::compound::NbtCompound;
use sculith_util::math::{
    boundingbox::BoundingBox,
    vector3::{Axis, Vector3},
};

use crate::{
    block::blocks::bubble_column::SURFACE_UPWARD_ACCELERATION,
    server::{Server, TickDeadline},
    world::{
        World,
        entity_islands::{self, AuditedReach},
    },
};

use super::{Entity, EntityBase, living::LivingEntity, player::Player, weak_slot::WeakSlot};

pub struct ExperienceOrbEntity {
    entity: Entity,
    amount: AtomicU32,
    health: AtomicI32,
    orb_age: AtomicU32,
    following_player: WeakSlot<Player>,
}

impl ExperienceOrbEntity {
    pub(super) const GROUND_FRICTION_OFFSET: f64 = 0.999_999f32 as f64;

    pub(super) fn velocity_after_motion(
        &self,
        moved: Vector3<f64>,
        attempted_y: f64,
        ground_slipperiness: Option<f32>,
    ) -> Vector3<f64> {
        let friction = Self::AIR_DRAG * ground_slipperiness.unwrap_or(1.0);
        let mut velocity = moved * f64::from(friction);
        if ground_slipperiness.is_some() && attempted_y < -self.get_gravity() {
            velocity.y = -attempted_y * 0.4;
        }
        velocity
    }

    pub(super) const AIR_DRAG: f32 = 0.98;
    pub(super) const FOLLOW_RANGE: f64 = 8.0;
    const DEFAULT_HEALTH: i32 = 5;
    const LIFETIME: u32 = 6_000;

    pub fn new(entity: Entity, amount: u32) -> Self {
        entity.yaw.store(rand::random::<f32>() * 360.0);
        Self {
            entity,
            amount: AtomicU32::new(amount),
            health: AtomicI32::new(Self::DEFAULT_HEALTH),
            orb_age: AtomicU32::new(0),
            following_player: WeakSlot::new(),
        }
    }

    pub fn spawn(world: &Arc<World>, position: Vector3<f64>, amount: u32) {
        Self::spawn_with_direction(world, position, Vector3::new(0.0, 0.0, 0.0), amount);
    }

    fn new_with_direction(
        world: &Arc<World>,
        position: Vector3<f64>,
        direction: Vector3<f64>,
        amount: u32,
    ) -> Self {
        let entity = Entity::new(world.clone(), position, &EntityType::EXPERIENCE_ORB);
        let orb = Self::new(entity, amount);
        let mut velocity = Vector3::new(
            (rand::random::<f64>() * 0.2 - 0.1) * 2.0,
            rand::random::<f64>() * 0.2 * 2.0,
            (rand::random::<f64>() * 0.2 - 0.1) * 2.0,
        );
        if direction.length_squared() > 0.0 && direction.dot(&velocity) < 0.0 {
            velocity = velocity.multiply(-1.0, -1.0, -1.0);
        }
        let size = orb.entity.bounding_box.load().get_average_side_length();
        let offset = direction.normalize() * (size * 0.5);
        orb.entity.set_pos(position.add(&offset));
        orb.entity.set_velocity(velocity);
        if !world.is_space_empty(orb.entity.bounding_box.load()) {
            orb.unstuck_if_possible(size);
        }
        orb
    }

    fn unstuck_if_possible(&self, max_distance: f64) {
        let bounding_box = self.entity.bounding_box.load();
        let size = bounding_box.max - bounding_box.min;
        let center = (bounding_box.min + bounding_box.max) * 0.5;
        let half_distance = Vector3::new(max_distance, max_distance, max_distance) * 0.5;
        let allowed_centers = BoundingBox {
            min: center - half_distance,
            max: center + half_distance,
        };
        let world = self.entity.world.load();
        let (mut collisions, _) =
            world.get_block_collisions(allowed_centers.expand(size.x, size.y, size.z), self);
        {
            let border = world
                .worldborder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let half = border.new_diameter * 0.5;
            let epsilon = f64::from(1.0e-5f32);
            collisions.retain(|shape| {
                shape.min.x >= border.center_x - half
                    && shape.max.x - epsilon < border.center_x + half
                    && shape.min.z >= border.center_z - half
                    && shape.max.z - epsilon < border.center_z + half
            });
        };
        let expanded_collisions = collisions
            .into_iter()
            .map(|shape| shape.expand(size.x * 0.5, size.y * 0.5, size.z * 0.5));
        if let Some(position) = closest_free_center(allowed_centers, center, expanded_collisions) {
            self.entity
                .set_pos(position.add_raw(0.0, -size.y * 0.5, 0.0));
        }
    }

    pub fn spawn_with_direction(
        world: &Arc<World>,
        position: Vector3<f64>,
        direction: Vector3<f64>,
        amount: u32,
    ) {
        let mut amount = amount;
        while amount > 0 {
            let i = Self::round_to_orb_size(amount);
            amount -= i;
            let orb = Arc::new(Self::new_with_direction(world, position, direction, i));
            world.spawn_entity(orb);
        }
    }

    /// Vanilla `ExperienceOrb.followNearbyPlayer`
    /// accelerate towards a player within 8 blocks.
    fn follow_nearby_player(&self) {
        let entity = &self.entity;
        let pos = entity.pos.load();
        let lost = self.following_player.get().is_none_or(|player| {
            player.is_spectator()
                || player.get_entity().pos.load().squared_distance_to_vec(&pos)
                    > Self::FOLLOW_RANGE * Self::FOLLOW_RANGE
        });
        if lost {
            let nearest =
                entity
                    .world
                    .load()
                    .get_nearest_player(pos, Self::FOLLOW_RANGE, |player| {
                        !player.is_spectator() && !player.living_entity.is_dead_or_dying()
                    });
            match nearest {
                Some(player) => self.following_player.set(&player),
                None => self.following_player.clear(),
            }
        }
        let Some(player) = self.following_player.get() else {
            return;
        };

        let target = player.get_entity();
        let target_pos = target
            .pos
            .load()
            .add_raw(0.0, target.get_eye_height() / 2.0, 0.0);
        let delta = target_pos.sub(&pos);
        let power = 1.0 - delta.length() / Self::FOLLOW_RANGE;
        entity
            .velocity
            .store(entity.velocity.load() + delta.normalize() * (power * power * 0.1));
        entity.velocity_dirty.store(true, Ordering::SeqCst);
    }

    pub(super) fn has_follow_target(&self) -> bool {
        self.following_player.get().is_some()
    }

    pub(crate) fn has_expired(&self) -> bool {
        self.orb_age.load(Ordering::Relaxed) > Self::LIFETIME
    }

    pub(super) fn advance_age(&self) -> u32 {
        self.orb_age.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub(crate) fn tick_from_world(
        &self,
        caller: &dyn EntityBase,
        server: &Server,
        deadline: Option<TickDeadline>,
    ) {
        let entity = &self.entity;
        let (_permit, advance) = entity.begin_world_motion(deadline);
        if entity.is_removed() {
            return;
        }
        if self.has_expired() {
            entity.remove();
        } else if advance {
            entity.tick_count.fetch_add(1, Ordering::Relaxed);
            if entity.has_vehicle() {
                entity.velocity.store(Vector3::default());
            }
            self.tick(caller, server);
        }
    }

    const fn round_to_orb_size(value: u32) -> u32 {
        if value >= 2477 {
            2477
        } else if value >= 1237 {
            1237
        } else if value >= 617 {
            617
        } else if value >= 307 {
            307
        } else if value >= 149 {
            149
        } else if value >= 73 {
            73
        } else if value >= 37 {
            37
        } else if value >= 17 {
            17
        } else if value >= 7 {
            7
        } else if value >= 3 {
            3
        } else {
            1
        }
    }
}

fn closest_free_center(
    allowed_centers: BoundingBox,
    center: Vector3<f64>,
    collisions: impl Iterator<Item = BoundingBox>,
) -> Option<Vector3<f64>> {
    // Subtract expanded collision boxes, matching vanilla's ONLY_FIRST shape operation.
    let mut free_boxes = vec![allowed_centers];
    for collision in collisions {
        let mut remaining = Vec::new();
        for mut free in free_boxes {
            if !free.intersects(&collision) {
                remaining.push(free);
                continue;
            }
            for axis in [Axis::X, Axis::Y, Axis::Z] {
                if free.min.get_axis(axis) < collision.min.get_axis(axis) {
                    let mut piece = free;
                    piece.max.set_axis(axis, collision.min.get_axis(axis));
                    remaining.push(piece);
                    free.min.set_axis(axis, collision.min.get_axis(axis));
                }
                if free.max.get_axis(axis) > collision.max.get_axis(axis) {
                    let mut piece = free;
                    piece.min.set_axis(axis, collision.max.get_axis(axis));
                    remaining.push(piece);
                    free.max.set_axis(axis, collision.max.get_axis(axis));
                }
            }
        }
        free_boxes = remaining;
    }
    free_boxes
        .into_iter()
        .map(|free| {
            Vector3::new(
                center.x.clamp(free.min.x, free.max.x),
                center.y.clamp(free.min.y, free.max.y),
                center.z.clamp(free.min.z, free.max.z),
            )
        })
        .min_by(|a, b| {
            a.squared_distance_to_vec(&center)
                .total_cmp(&b.squared_distance_to_vec(&center))
        })
}

impl EntityBase for ExperienceOrbEntity {
    /// Vanilla `ExperienceOrb.doWaterSplashEffect` does nothing.
    fn do_water_splash_effect(&self, _caller: &dyn EntityBase) {}

    fn audited_reach(&self) -> Option<AuditedReach> {
        let entity = &self.entity;
        let world = entity.world.load();
        let fluid_push = super::max_fluid_push_speed(world.dimension.fast_lava);
        let velocity = entity.velocity.load();
        let mut reach = AuditedReach {
            bounding_box: entity.bounding_box.load(),
            motion: (
                velocity.x.hypot(velocity.z) + fluid_push,
                velocity.y.abs() + self.get_gravity() + SURFACE_UPWARD_ACCELERATION + fluid_push,
            ),
            entities: 0.0,
            players: 0.0,
            block_reads: 1.0,
            route: None,
            changes_blocks: false,
            global: false,
            links: entity_islands::links_of(self),
        };
        reach.scan_contact_blocks(&world);
        Some(reach)
    }

    fn write_custom_nbt(&self, nbt: &mut NbtCompound) {
        nbt.put_short("Health", self.health.load(Ordering::Relaxed) as i16);
        nbt.put_short("Age", self.orb_age.load(Ordering::Relaxed) as i16);
        let value = self.amount.load(Ordering::Relaxed);
        if let Ok(short) = i16::try_from(value) {
            nbt.put_short("Value", short);
        } else {
            nbt.put_long("Value", i64::from(value));
        }
    }

    fn read_custom_nbt(&self, nbt: &NbtCompound) {
        let _motion = self.entity.hold_actor_motion();
        self.health.store(
            nbt.get_short("Health")
                .map_or(Self::DEFAULT_HEALTH, i32::from),
            Ordering::Relaxed,
        );
        if let Some(age) = orb_nbt_integer(nbt, "Age") {
            let age = age.clamp(0, i64::from(Self::LIFETIME + 1)) as u32;
            self.orb_age.store(age, Ordering::Relaxed);
        }
        if let Some(value) =
            orb_nbt_integer(nbt, "Value").and_then(|value| u32::try_from(value).ok())
        {
            self.amount.store(value, Ordering::Relaxed);
        }
    }

    fn tick(&self, caller: &dyn EntityBase, server: &Server) {
        let entity = &self.entity;
        entity.tick(caller, server);
        let bounding_box = entity.bounding_box.load();

        let original_velo = entity.velocity.load();

        let mut velo = original_velo;

        let no_physics = !self
            .entity
            .world
            .load()
            .is_space_empty(bounding_box.expand(-1.0e-7, -1.0e-7, -1.0e-7));
        self.entity.no_physics.store(no_physics, Ordering::Relaxed);
        // TODO: isSubmergedIn
        if !no_physics {
            velo.y -= self.get_gravity();
        }

        entity.velocity.store(velo);
        self.follow_nearby_player();
        let velo = entity.velocity.load();

        entity.move_entity(caller, velo);

        entity.tick_block_collisions(caller);

        let slipperiness = entity.on_ground.load(Ordering::Relaxed).then(|| {
            let (pos, ..) = entity.get_pos_with_y_offset(Self::GROUND_FRICTION_OFFSET);
            entity.world.load().get_block(&pos).slipperiness
        });
        entity.velocity.store(self.velocity_after_motion(
            entity.velocity.load(),
            velo.y,
            slipperiness,
        ));

        if self.advance_age() > Self::LIFETIME {
            entity.remove();
        }
    }

    fn get_entity(&self) -> &Entity {
        &self.entity
    }

    fn receives_player_touch(&self) -> bool {
        true
    }

    fn damage_with_context(
        &self,
        _caller: &dyn EntityBase,
        amount: f32,
        damage_type: DamageType,
        _position: Option<Vector3<f64>>,
        _source: Option<&dyn EntityBase>,
        cause: Option<&dyn EntityBase>,
    ) -> bool {
        let _motion = self.entity.hold_actor_motion();
        if self.entity.is_removed() || self.entity.is_invulnerable_to(&damage_type, cause) {
            return false;
        }

        let mut event = crate::plugin::api::events::entity::entity_damage::EntityDamageEvent::new(
            self.entity.entity_id,
            damage_type,
            amount,
        );
        if let Some(server) = self.entity.world.load().server.upgrade() {
            server.plugin_manager.fire_blocking(&server, &mut event);
        }
        if event.cancelled {
            return false;
        }
        if !event.damage.is_finite() || event.damage <= 0.0 {
            return false;
        }

        self.entity.velocity_dirty.store(true, Ordering::Relaxed);
        let mut health = self.health.load(Ordering::Relaxed);
        loop {
            let remaining = (health as f32 - event.damage) as i32;
            match self.health.compare_exchange_weak(
                health,
                remaining,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    if remaining <= 0 {
                        self.entity.remove();
                    }
                    return true;
                }
                Err(current) => health = current,
            }
        }
    }

    fn on_player_collision(&self, player: &Arc<Player>) {
        let _motion = self.entity.hold_actor_motion();
        if !self.entity.is_removed() && !self.has_expired() && player.begin_experience_pickup() {
            player.living_entity.pickup(&self.entity, 1);
            self.entity.remove();
            let amount = self.amount.load(Ordering::Relaxed) as i32;
            let remaining = player.apply_mending_from_xp(amount);
            if remaining > 0 {
                player.add_experience_points(remaining);
            }
        }
    }

    fn get_living_entity(&self) -> Option<&LivingEntity> {
        None
    }
    fn get_gravity(&self) -> f64 {
        0.03
    }

    fn cast_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn orb_nbt_integer(nbt: &NbtCompound, name: &str) -> Option<i64> {
    nbt.get_short(name)
        .map(i64::from)
        .or_else(|| nbt.get_int(name).map(i64::from))
        .or_else(|| nbt.get_long(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::VanillaData;
    use pumpkin_data::Block;
    use pumpkin_world::{chunk::ChunkData, world::BlockFlags};
    use sculith_config::{
        AdvancedConfiguration, BasicConfiguration, telemetry::TelemetryConfig, world::WorldPreset,
    };
    use sculith_util::math::position::BlockPos;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn regional_floor_steps_match_world_landing_bounce_and_drag() {
        let folder = tempfile::tempdir().unwrap();
        let basic = BasicConfiguration {
            default_level_name: folder.path().join("world").to_string_lossy().into_owned(),
            allow_nether: false,
            allow_end: false,
            ..Default::default()
        };
        let mut advanced = AdvancedConfiguration::default();
        advanced.world.generator = WorldPreset::Flat;
        let server = Server::new(
            basic,
            advanced,
            TelemetryConfig::default(),
            VanillaData::default(),
            Vec::new(),
        )
        .await
        .unwrap();
        let world = server.worlds.load()[0].clone();
        let make_orb = |x| {
            let position = Vector3::new(x + 0.5, 65.125, 8.5);
            let floor = BlockPos::new(x as i32, 64, 8);
            let chunk = floor.chunk_position();
            world
                .level
                .loaded_chunks
                .insert(chunk, ChunkData::empty_sync(chunk.x, chunk.y));
            world.set_block_state(
                &floor,
                Block::STONE.default_state.id,
                BlockFlags::FORCE_STATE,
            );
            let orb = Arc::new(ExperienceOrbEntity::new(
                Entity::new(world.clone(), position, &EntityType::EXPERIENCE_ORB),
                17,
            ));
            orb.entity.velocity.store(Vector3::new(0.125, -0.25, 0.0));
            orb
        };
        let ordinary = make_orb(8.0);
        let regional = make_orb(24.0);
        world
            .entities
            .store(Arc::new(vec![ordinary.clone(), regional.clone()]));
        let chunk = regional.entity.chunk_pos.load();
        let start = Instant::now();
        let period = Duration::from_millis(50);
        for tick in 0..6 {
            let step = TickDeadline {
                at: start + period * tick,
                period,
            };
            ordinary.tick_from_world(ordinary.as_ref(), &server, None);
            regional.try_regional_orb_tick(&world, chunk, 0, step, &|pos| pos == chunk, || {});
            regional.try_regional_orb_tick(&world, chunk, 0, step, &|pos| pos == chunk, || {});
            assert_eq!(regional.orb_age.load(Ordering::Relaxed), tick + 1);
            let a = ordinary.entity.pos.load().add_raw(16.0, 0.0, 0.0);
            assert!(a.squared_distance_to_vec(&regional.entity.pos.load()) < 1e-18);
            assert_eq!(
                ordinary.entity.velocity.load(),
                regional.entity.velocity.load()
            );
            assert_eq!(
                ordinary.entity.on_ground.load(Ordering::Relaxed),
                regional.entity.on_ground.load(Ordering::Relaxed),
            );
            if tick == 0 {
                assert!(regional.entity.on_ground.load(Ordering::Relaxed));
                assert!(regional.entity.velocity.load().y > 0.0);
            }
        }
        server.shutdown().await;
    }

    #[test]
    fn unstuck_selects_nearest_free_center_and_leaves_enclosed_orbs_in_place() {
        let allowed = BoundingBox {
            min: Vector3::new(0.0, 0.0, 0.0),
            max: Vector3::new(1.0, 1.0, 1.0),
        };
        let center = Vector3::new(0.5, 0.5, 0.5);
        let wall = BoundingBox {
            min: Vector3::new(-1.0, -1.0, -1.0),
            max: Vector3::new(0.75, 2.0, 2.0),
        };
        let floor = BoundingBox {
            min: Vector3::new(-1.0, -1.0, -1.0),
            max: Vector3::new(2.0, 0.625, 2.0),
        };
        assert_eq!(
            closest_free_center(allowed, center, [wall, floor].into_iter()),
            Some(Vector3::new(0.75, 0.625, 0.5))
        );
        assert_eq!(
            closest_free_center(allowed, center, [allowed].into_iter()),
            None
        );
    }
}
