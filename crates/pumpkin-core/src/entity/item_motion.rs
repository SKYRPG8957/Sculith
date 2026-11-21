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

use super::{
    Collision, Entity,
    item::{ItemClock, ItemEntity, ItemTimerOwner},
};

#[derive(Default)]
pub(super) struct ItemMotion {
    clock: ItemClock,
    world_users: usize,
}

struct ItemMotionUpdate {
    position: Vector3<f64>,
    bounds: BoundingBox,
    collision: Option<Collision>,
    velocity: Vector3<f64>,
}

impl ItemMotionUpdate {
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

pub struct ItemWorldAccess<'a>(&'a Mutex<ItemMotion>);

impl Drop for ItemWorldAccess<'_> {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .world_users -= 1;
    }
}

impl Entity {
    /// Prevents a regional move while a world operation may touch the actor.
    pub(crate) fn hold_item_motion(&self) -> Option<ItemWorldAccess<'_>> {
        let motion = self.item_motion.as_deref()?;
        motion
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .world_users += 1;
        Some(ItemWorldAccess(motion))
    }

    pub(super) fn begin_item_world_tick(
        &self,
        deadline: Option<TickDeadline>,
    ) -> (Option<ItemWorldAccess<'_>>, bool) {
        let Some(motion) = self.item_motion.as_deref() else {
            return (None, true);
        };
        let mut state = motion.lock().unwrap_or_else(PoisonError::into_inner);
        state.world_users += 1;
        let advance = if let Some(deadline) = deadline {
            state.clock.claim(ItemTimerOwner::World, deadline)
        } else {
            state.clock = ItemClock::default();
            true
        };
        (Some(ItemWorldAccess(motion)), advance)
    }
}

impl ItemEntity {
    pub(crate) fn try_regional_expiry(&self, discard: impl FnOnce()) {
        let entity = self.get_entity();
        let Some(motion) = entity.item_motion.as_deref() else {
            return;
        };
        let Ok(motion) = motion.try_lock() else {
            return;
        };
        if motion.world_users != 0 || !self.has_expired() {
            return;
        }
        let (Ok(vehicle), Ok(passengers)) =
            (entity.vehicle.try_lock(), entity.passengers.try_lock())
        else {
            return;
        };
        if vehicle.is_none() && passengers.is_empty() {
            discard();
        }
    }

    pub(crate) fn try_regional_motion(
        &self,
        world: &World,
        chunk: Vector2<i32>,
        owner: RegionId,
        deadline: TickDeadline,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) {
        let entity = self.get_entity();
        let Some(motion) = entity.item_motion.as_deref() else {
            return;
        };
        let Ok(mut motion) = motion.try_lock() else {
            return;
        };
        let clock_owner = ItemTimerOwner::Region(owner);
        if motion.world_users != 0 || !motion.clock.can_claim(clock_owner, deadline) {
            return;
        }
        if entity.is_removed()
            || !std::ptr::eq(entity.world.load().as_ref(), world)
            || entity.chunk_pos.load() != chunk
            || entity.no_physics.load(Relaxed)
            || entity.is_on_fire()
            || entity.touching_water.load(Relaxed)
            || entity.touching_lava.load(Relaxed)
            || entity.portal_cooldown.load(Relaxed) != 0
            || entity.riding_cooldown.load(Relaxed) != 0
            || entity.movement_multiplier.load() != Vector3::default()
        {
            return;
        }
        let (Ok(vehicle), Ok(passengers), Ok(portal)) = (
            entity.vehicle.try_lock(),
            entity.passengers.try_lock(),
            entity.portal_manager.try_lock(),
        ) else {
            return;
        };
        if vehicle.is_some() || !passengers.is_empty() || portal.is_some() {
            return;
        }
        // The shared-index path may run beside other bounds writers. Only this item's
        // owner is pinned exclusively, using the usual bounds-then-index lock order.
        let Ok(bounds_owner) = entity.bounding_box.index.try_write() else {
            return;
        };
        if !std::ptr::eq(bounds_owner.as_ptr(), Arc::as_ptr(&world.entity_index)) {
            return;
        }

        self.try_apply_regional_motion(world, owns, || {
            owns(chunk) && motion.clock.claim(clock_owner, deadline)
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
                let step = ItemMotionUpdate {
                    position,
                    bounds: next_bounds,
                    collision: moves.then_some(collision),
                    velocity,
                };
                Some((BlockPos::floored_v(position), step))
            },
            |step, biome| {
                world.entity_index.try_move_isolated_item(
                    world,
                    entity.entity_id,
                    // Match the pickup reach in the world phase, which also covers merge distance.
                    swept.expand(1.0, 0.5, 1.0),
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
