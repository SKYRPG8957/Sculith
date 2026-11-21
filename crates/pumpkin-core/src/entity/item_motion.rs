use std::sync::{Mutex, PoisonError, atomic::Ordering::Relaxed};

use pumpkin_util::math::{position::BlockPos, vector2::Vector2, vector3::Vector3};

use crate::{
    server::TickDeadline,
    world::{COLLISION_LOOK_BELOW, RegionId, World},
};

use super::{
    Entity,
    item::{ItemClock, ItemEntity, ItemTimerOwner},
};

#[derive(Default)]
pub(super) struct ItemMotion {
    clock: ItemClock,
    world_users: usize,
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
            || entity.on_ground.load(Relaxed)
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

        let original_velocity = entity.velocity.load();
        let movement = self.apply_fluid_drag_or_gravity(original_velocity);
        // A short sweep bounds palette work even when an outside velocity is applied.
        let distance_squared = movement.length_squared();
        if !(0.0 < distance_squared && distance_squared <= 1.0) {
            return;
        }
        let position = entity.pos.load() + movement;
        if !position.x.is_finite() || !position.y.is_finite() || !position.z.is_finite() {
            return;
        }
        let destination = BlockPos::floored_v(position);
        let next_bounds = Entity::make_bounding_box(
            entity.entity_type,
            position,
            &entity.entity_dimension.load(),
        );
        let swept = entity
            .bounding_box
            .load()
            .stretch(movement)
            .minmax(&next_bounds);
        let min = BlockPos::floored_v(swept.min.add_raw(0.0, -COLLISION_LOOK_BELOW, 0.0));
        let max = swept.max_block_pos();
        world
            .level
            .try_with_clear_regional_path(min, max, destination, owns, |biome| {
                world.entity_index.try_move_isolated_item(
                    world,
                    entity.entity_id,
                    // Match the pickup reach in the world phase, which also covers merge distance.
                    swept.expand(1.0, 0.5, 1.0),
                    next_bounds,
                    || {
                        if !owns(chunk) {
                            return false;
                        }
                        motion.clock.claim(clock_owner, deadline);
                        entity.update_last_pos();
                        entity.tick_count.fetch_add(1, Relaxed);
                        entity.first_tick.store(false, Relaxed);
                        entity
                            .was_in_powder_snow
                            .store(entity.is_in_powder_snow.swap(false, Relaxed), Relaxed);
                        entity.water_height.store(0.0);
                        entity.lava_height.store(0.0);
                        entity.supporting_block_pos.store(None);
                        entity.horizontal_collision.store(false, Relaxed);
                        entity.vertical_collision.store(false, Relaxed);
                        entity.set_pos_with(
                            position,
                            || entity.bounding_box.value.store(next_bounds),
                            |_| biome,
                        );
                        entity.velocity.store(movement * Self::AIR_DRAG);
                        self.mark_velocity_change(original_velocity, false);
                        true
                    },
                );
            });
    }
}
