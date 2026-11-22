use std::sync::{
    Arc, Mutex, PoisonError,
    atomic::Ordering::{Relaxed, SeqCst},
};

use pumpkin_data::{BlockStateId, biome::Biome, fluid::Fluid};
use pumpkin_util::math::{
    boundingbox::BoundingBox, position::BlockPos, vector2::Vector2, vector3::Vector3,
};

use crate::{
    server::TickDeadline,
    world::{COLLISION_LOOK_BELOW, RegionId, World},
};

use super::{Collision, Entity, EntityBase, experience_orb::ExperienceOrbEntity, item::ItemEntity};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DropTickOwner {
    World,
    Region(RegionId),
}

#[derive(Default)]
pub(super) struct DropClock {
    last: Option<(DropTickOwner, std::time::Instant)>,
}

impl DropClock {
    fn can_claim(&self, owner: DropTickOwner, deadline: TickDeadline) -> bool {
        self.last.is_none_or(|(previous, at)| {
            deadline.at > at && (previous == owner || deadline.at - at >= deadline.period)
        })
    }

    pub(super) fn claim(&mut self, owner: DropTickOwner, deadline: TickDeadline) -> bool {
        if !self.can_claim(owner, deadline) {
            return false;
        }
        self.last = Some((owner, deadline.at));
        true
    }
}

#[derive(Default)]
pub(super) struct DropMotion {
    clock: DropClock,
    world_users: usize,
}

struct DropMotionUpdate {
    position: Vector3<f64>,
    bounds: BoundingBox,
    collision: Option<Collision>,
    velocity: Vector3<f64>,
}

impl DropMotionUpdate {
    fn apply(self, entity: &Entity, biome: &'static Biome) {
        entity.update_last_pos();
        entity.tick_count.fetch_add(1, Relaxed);
        entity.first_tick.store(false, Relaxed);
        entity
            .was_in_powder_snow
            .store(entity.is_in_powder_snow.swap(false, Relaxed), Relaxed);
        entity.water_height.store(0.0);
        entity.lava_height.store(0.0);
        if let Some(collision) = self.collision {
            entity
                .on_ground
                .store(collision.supporting.is_some(), SeqCst);
            entity.supporting_block_pos.store(collision.supporting);
            entity
                .horizontal_collision
                .store(collision.horizontal, Relaxed);
            entity.vertical_collision.store(collision.vertical, Relaxed);
            entity.set_pos_with(
                self.position,
                || entity.bounding_box.value.store(self.bounds),
                |_| biome,
            );
        }
        entity.velocity.store(self.velocity);
    }
}

pub struct DropWorldAccess<'a>(&'a Mutex<DropMotion>);

impl Drop for DropWorldAccess<'_> {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .world_users -= 1;
    }
}

impl Entity {
    /// Prevents a regional move while a world operation may touch the actor.
    pub(crate) fn hold_drop_motion(&self) -> Option<DropWorldAccess<'_>> {
        let motion = self.drop_motion.as_deref()?;
        motion
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .world_users += 1;
        Some(DropWorldAccess(motion))
    }

    pub(super) fn begin_drop_world_tick(
        &self,
        deadline: Option<TickDeadline>,
    ) -> (Option<DropWorldAccess<'_>>, bool) {
        let Some(motion) = self.drop_motion.as_deref() else {
            return (None, true);
        };
        let mut state = motion.lock().unwrap_or_else(PoisonError::into_inner);
        state.world_users += 1;
        let advance = if let Some(deadline) = deadline {
            state.clock.claim(DropTickOwner::World, deadline)
        } else {
            state.clock = DropClock::default();
            true
        };
        (Some(DropWorldAccess(motion)), advance)
    }

    pub(crate) fn start_regional_drop_motion(&self, owner: RegionId, deadline: TickDeadline) {
        if let Some(motion) = self.drop_motion.as_deref() {
            motion
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clock
                .claim(DropTickOwner::Region(owner), deadline);
        }
    }
    pub(crate) fn try_regional_drop_expiry(&self, discard: impl FnOnce()) {
        let Some(motion) = self.drop_motion.as_deref() else {
            return;
        };
        let Ok(motion) = motion.try_lock() else {
            return;
        };
        if motion.world_users != 0 {
            return;
        }
        let (Ok(vehicle), Ok(passengers)) = (self.vehicle.try_lock(), self.passengers.try_lock())
        else {
            return;
        };
        if vehicle.is_none() && passengers.is_empty() {
            discard();
        }
    }

    fn with_regional_drop_motion(
        &self,
        world: &World,
        chunk: Vector2<i32>,
        owner: RegionId,
        deadline: TickDeadline,
        advance: impl FnOnce(&mut DropClock),
    ) {
        let Some(motion) = self.drop_motion.as_deref() else {
            return;
        };
        let Ok(mut motion) = motion.try_lock() else {
            return;
        };
        let clock_owner = DropTickOwner::Region(owner);
        if motion.world_users != 0 || !motion.clock.can_claim(clock_owner, deadline) {
            return;
        }
        if self.is_removed()
            || !std::ptr::eq(self.world.load().as_ref(), world)
            || self.chunk_pos.load() != chunk
            || self.no_physics.load(Relaxed)
            || self.is_on_fire()
            || self.fire_ticks.load(Relaxed) > 0
            || self.touching_water.load(Relaxed)
            || self.touching_lava.load(Relaxed)
            || self.portal_cooldown.load(Relaxed) != 0
            || self.riding_cooldown.load(Relaxed) != 0
            || self.movement_multiplier.load() != Vector3::default()
        {
            return;
        }
        let (Ok(vehicle), Ok(passengers), Ok(portal)) = (
            self.vehicle.try_lock(),
            self.passengers.try_lock(),
            self.portal_manager.try_lock(),
        ) else {
            return;
        };
        if vehicle.is_some() || !passengers.is_empty() || portal.is_some() {
            return;
        }
        // The shared-index path may run beside other bounds writers. Only this actor's
        // owner is pinned exclusively, using the usual bounds-then-index lock order.
        let Ok(bounds_owner) = self.bounding_box.index.try_write() else {
            return;
        };
        if !std::ptr::eq(bounds_owner.as_ptr(), Arc::as_ptr(&world.entity_index)) {
            return;
        }

        advance(&mut motion.clock);
    }
}

impl ItemEntity {
    pub(crate) fn try_regional_motion(
        &self,
        world: &World,
        chunk: Vector2<i32>,
        owner: RegionId,
        deadline: TickDeadline,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) {
        self.get_entity()
            .with_regional_drop_motion(world, chunk, owner, deadline, |clock| {
                self.try_apply_regional_motion(world, owns, || {
                    owns(chunk) && clock.claim(DropTickOwner::Region(owner), deadline)
                });
            });
    }

    fn try_apply_regional_motion(
        &self,
        world: &World,
        owns: &impl Fn(Vector2<i32>) -> bool,
        claim: impl FnOnce() -> bool,
    ) {
        let entity = self.get_entity();
        let original_velocity = entity.velocity.load();
        let velocity = self.apply_fluid_drag_or_gravity(original_velocity);
        let next_tick = entity.tick_count.load(Relaxed).wrapping_add(1);
        let moves = self.should_tick_move(velocity, next_tick);
        // A short sweep bounds palette work even when an outside velocity is applied.
        let distance_squared = velocity.length_squared();
        if !((0.0..=1.0).contains(&distance_squared) && (!moves || distance_squared > 0.0)) {
            return;
        }
        let movement = if moves { velocity } else { Vector3::default() };
        let origin = entity.pos.load();
        let attempted_position = origin + movement;
        if !attempted_position.x.is_finite()
            || !attempted_position.y.is_finite()
            || !attempted_position.z.is_finite()
        {
            return;
        }
        let bounds = entity.bounding_box.load();
        let dimensions = entity.entity_dimension.load();
        let attempted_bounds =
            Entity::make_bounding_box(entity.entity_type, attempted_position, &dimensions);
        let swept = bounds.stretch(movement).minmax(&attempted_bounds);
        let min = BlockPos::floored_v(swept.min.add_raw(0.0, -COLLISION_LOOK_BELOW, 0.0));
        let max = swept.max_block_pos();
        world.level.try_with_regional_path(
            min,
            max,
            owns,
            |read| {
                let (collision, slipperiness) =
                    regional_item_collision(world, bounds, movement, min, max, read)?;
                let position = origin + collision.movement;
                let next_bounds = if position == origin {
                    bounds
                } else if position == attempted_position {
                    attempted_bounds
                } else {
                    Entity::make_bounding_box(entity.entity_type, position, &dimensions)
                };
                let velocity = if moves {
                    regional_item_velocity(movement, &collision, slipperiness)
                } else {
                    velocity
                };
                let step = DropMotionUpdate {
                    position,
                    bounds: next_bounds,
                    collision: moves.then_some(collision),
                    velocity,
                };
                Some((BlockPos::floored_v(position), step))
            },
            |step, biome| {
                // Match the pickup reach in the world phase, which also covers merge distance.
                let area = swept.expand(1.0, 0.5, 1.0);
                world.entity_index.try_move_isolated_drop(
                    world,
                    entity.entity_id,
                    area,
                    area,
                    step.bounds,
                    || {
                        if !claim() {
                            return false;
                        }
                        step.apply(entity, biome);
                        self.mark_velocity_change(original_velocity, false);
                        true
                    },
                );
            },
        );
    }
}

impl ExperienceOrbEntity {
    pub(crate) fn try_regional_orb_tick(
        &self,
        world: &World,
        chunk: Vector2<i32>,
        owner: RegionId,
        deadline: TickDeadline,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) {
        let entity = self.get_entity();
        entity.with_regional_drop_motion(world, chunk, owner, deadline, |clock| {
            if self.has_expired() || self.has_follow_target() || entity.on_ground.load(Relaxed) {
                return;
            }
            let movement = entity
                .velocity
                .load()
                .add_raw(0.0, -self.get_gravity(), 0.0);
            let distance_squared = movement.length_squared();
            if !(distance_squared > 0.0 && distance_squared <= 1.0) {
                return;
            }
            let position = entity.pos.load() + movement;
            let bounds = Entity::make_bounding_box(
                entity.entity_type,
                position,
                &entity.entity_dimension.load(),
            );
            let swept = entity.bounding_box.load().minmax(&bounds);
            let min = BlockPos::floored_v(swept.min.add_raw(0.0, -COLLISION_LOOK_BELOW, 0.0));
            let max = swept.max_block_pos();
            let destination = BlockPos::floored_v(position);
            world
                .level
                .try_with_clear_regional_path(min, max, destination, owns, |biome| {
                    let radius = Self::FOLLOW_RANGE;
                    world.entity_index.try_move_isolated_drop(
                        world,
                        entity.entity_id,
                        swept,
                        swept.expand(radius, radius, radius),
                        bounds,
                        || {
                            if !owns(chunk) || !clock.claim(DropTickOwner::Region(owner), deadline)
                            {
                                return false;
                            }
                            DropMotionUpdate {
                                position,
                                bounds,
                                collision: Some(Collision::free(movement)),
                                velocity: movement * f64::from(Self::AIR_DRAG),
                            }
                            .apply(entity, biome);
                            let age = self.advance_age();
                            tracing::trace!(
                                entity = entity.entity_id,
                                region = owner,
                                age,
                                "Advanced experience orb on its region"
                            );
                            true
                        },
                    );
                });
        });
    }
}

/// Air and inert full blocks below the item need no fluid, contact or fall callbacks.
#[expect(clippy::float_cmp)]
fn regional_item_collision(
    world: &World,
    bounds: BoundingBox,
    movement: Vector3<f64>,
    min: BlockPos,
    max: BlockPos,
    read: &dyn Fn(BlockPos) -> Option<BlockStateId>,
) -> Option<(Collision, Option<f32>)> {
    let mut shapes = Vec::new();
    let mut positions = Vec::new();
    let swept = bounds.stretch(movement);
    for position in BlockPos::iterate(min, max) {
        let id = read(position)?;
        let state = id.to_state();
        if state.is_air() {
            continue;
        }
        let floor = BoundingBox::from_block(&position);
        let block = id.to_block();
        if floor.max.y > bounds.min.y
            || block.get_speed_factor() != 1.0
            || block.bounce_restitution != 0.0
            || world.block_registry.get_pumpkin_block(block.id).is_some()
            || World::get_fluid_from_state_id(id).id != Fluid::EMPTY.id
        {
            return None;
        }
        let mut block_shapes = state.get_block_collision_shapes_at(&position);
        let shape = block_shapes.next()?.at_pos(position);
        if shape.min != floor.min || shape.max != floor.max || block_shapes.next().is_some() {
            return None;
        }
        if shape.intersects(&swept) {
            shapes.push(shape);
            positions.push((shapes.len(), position));
        }
    }
    let collision = Entity::collide_with_block_shapes(bounds, movement, &shapes, positions);
    let destination = bounds.shift(collision.movement);
    if shapes.iter().any(|shape| shape.intersects(&destination)) {
        return None;
    }
    let slipperiness = match collision.supporting {
        Some(floor) => Some(read(floor)?.to_block().slipperiness),
        None => None,
    };
    Some((collision, slipperiness))
}

#[expect(clippy::float_cmp)]
fn regional_item_velocity(
    movement: Vector3<f64>,
    collision: &Collision,
    slipperiness: Option<f32>,
) -> Vector3<f64> {
    let mut velocity = collision.movement;
    if movement.y != velocity.y {
        // The default full-block fall handler stops Y velocity without changing X or Z.
        velocity.y = 0.0;
    }
    ItemEntity::velocity_after_motion(velocity, slipperiness)
}
