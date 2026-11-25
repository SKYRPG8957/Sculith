use std::sync::atomic::Ordering::Relaxed;

use pumpkin_data::{attributes::Attributes, entity::EntityType};
use sculith_util::math::{position::BlockPos, vector2::Vector2, vector3::Vector3};

use crate::{
    entity::{Entity, EntityBase, drop_motion::ActorTickOwner, mob::Mob},
    server::TickDeadline,
    world::{RegionId, World},
};

use super::LivingEntity;

pub fn supports_regional_air_travel(kind: &EntityType) -> bool {
    kind == &EntityType::COW || kind == &EntityType::PIG || kind == &EntityType::SHEEP
}

impl LivingEntity {
    pub(crate) fn try_regional_air_travel(
        &self,
        caller: &dyn EntityBase,
        world: &World,
        owner: RegionId,
        deadline: TickDeadline,
        owns: &impl Fn(Vector2<i32>) -> bool,
        publish: impl FnOnce(),
    ) {
        let chunk = self.entity.chunk_pos.load();
        if !owns(chunk) {
            return;
        }
        self.entity.with_regional_motion(
            world,
            chunk,
            owner,
            deadline,
            |clock| {
                self.try_apply_air_travel(caller, world, owns, || {
                    owns(chunk) && clock.claim(ActorTickOwner::Region(owner), deadline)
                });
            },
            publish,
        );
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Keeps motion checks and the pinned commit together"
    )]
    fn try_apply_air_travel(
        &self,
        caller: &dyn EntityBase,
        world: &World,
        owns: &impl Fn(Vector2<i32>) -> bool,
        claim: impl FnOnce() -> bool,
    ) {
        let entity = &self.entity;
        if entity.first_tick.load(Relaxed)
            || entity.on_ground.load(Relaxed)
            || entity.has_no_gravity()
            || entity.fall_flying.load(Relaxed)
            || entity.is_in_powder_snow.load(Relaxed)
            || entity.was_in_powder_snow.load(Relaxed)
            || self.jumping.load(Relaxed)
            || self.climbing.load(Relaxed)
            || !caller.is_effective_ai()
            || caller.is_immobile()
            || caller.is_flapping()
        {
            return;
        }
        let (Ok(effects), Ok(leashed), Ok(delayed_leash)) = (
            self.active_effects.try_lock(),
            entity.leashed_to.try_lock(),
            entity.delayed_leash.try_lock(),
        ) else {
            return;
        };
        if !effects.is_empty() || leashed.is_some() || delayed_leash.is_some() {
            return;
        }

        let movement = entity.velocity_after_cutoff();
        // Bound the span of the palette sweep for large external impulses.
        if !(-2.0..=-0.003).contains(&movement.y)
            || !(0.0..=1.0).contains(&movement.horizontal_length_squared())
            || self.movement_input.load() != Vector3::default()
        {
            return;
        }
        // These mobs use the standard gravity attribute. Keep its value and
        // drag stable through the step without waiting for a world edit.
        let Ok(attributes) = self.attributes.try_read() else {
            return;
        };
        let gravity = Self::attribute_value_from(&attributes, &Attributes::GRAVITY);
        let modifier =
            Self::attribute_value_from(&attributes, &Attributes::AIR_DRAG_MODIFIER) as f32;
        let (velocity, _) =
            self.apply_air_drag(caller, movement.add_raw(0.0, -gravity, 0.0), modifier, 1.0);
        if !velocity.length_squared().is_finite() {
            return;
        }
        let position = entity.pos.load() + movement;
        if !position.x.is_finite() || !position.y.is_finite() || !position.z.is_finite() {
            return;
        }
        let bounds = entity.bounding_box.load();
        let next = Entity::make_bounding_box(
            entity.entity_type,
            position,
            &entity.entity_dimension.load(),
        );
        // The nearby layer under the feet can reset fall distance even without contact.
        let clearance = bounds.minmax(&next).expand(1.0, 1.0, 1.0);
        let min = BlockPos::floored_v(clearance.min);
        let max = clearance.max_block_pos();
        world.level.try_with_regional_path(
            min,
            max,
            owns,
            |read| {
                if BlockPos::iterate(min, max)
                    .any(|position| read(position).is_none_or(|id| !id.to_state().is_air()))
                {
                    return None;
                }
                let border = world.worldborder.try_lock().ok()?;
                if !border.contains(next.min.x, next.min.z)
                    || !border.contains(next.max.x, next.max.z)
                {
                    return None;
                }
                Some((BlockPos::floored_v(position), border))
            },
            |border, biome| {
                world.entity_index.try_update_isolated_actor_bounds(
                    world,
                    entity.entity_id,
                    clearance,
                    clearance,
                    next,
                    || {
                        if !claim() {
                            return false;
                        }
                        entity.update_last_pos();
                        entity.set_pos_with(
                            position,
                            || entity.bounding_box.value.store(next),
                            |_| biome,
                        );
                        entity.horizontal_collision.store(false, Relaxed);
                        entity.vertical_collision.store(false, Relaxed);
                        entity.supporting_block_pos.store(None);
                        entity.velocity.store(velocity);
                        if caller.get_mob().is_some_and(Mob::should_check_fall_damage) {
                            self.accumulate_fall_distance(movement.y);
                        }
                        if caller.movement_emission().emits_anything() {
                            let (walked, flown) = entity.movement_distances(movement, false);
                            entity.move_dist.store(walked);
                            entity.fly_dist.store(flown);
                        }
                        true
                    },
                );
                drop(border);
            },
        );
        drop((attributes, effects, leashed, delayed_leash));
    }
}
