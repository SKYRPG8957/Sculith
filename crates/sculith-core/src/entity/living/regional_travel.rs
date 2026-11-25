use std::sync::atomic::Ordering::Relaxed;

use pumpkin_data::{BlockStateId, attributes::Attributes, entity::EntityType};
use sculith_util::math::{
    boundingbox::BoundingBox, position::BlockPos, vector2::Vector2, vector3::Vector3,
};

use crate::{
    entity::{
        Entity, EntityBase,
        drop_motion::{ActorTickOwner, regional_floor_collision},
        mob::Mob,
    },
    server::TickDeadline,
    world::{COLLISION_LOOK_BELOW, RegionId, World},
};

use super::{EquipmentSlot, LivingEntity};

pub fn supports_regional_travel(kind: &EntityType) -> bool {
    kind == &EntityType::COW || kind == &EntityType::PIG || kind == &EntityType::SHEEP
}

impl LivingEntity {
    pub(crate) fn try_regional_travel(
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
                self.try_apply_regional_travel(caller, world, owns, || {
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
    fn try_apply_regional_travel(
        &self,
        caller: &dyn EntityBase,
        world: &World,
        owns: &impl Fn(Vector2<i32>) -> bool,
        claim: impl FnOnce() -> bool,
    ) {
        let entity = &self.entity;
        if entity.first_tick.load(Relaxed)
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
        let attribute = |kind: &Attributes| Self::attribute_value_from(&attributes, kind);
        if entity.on_ground.load(Relaxed) {
            self.try_apply_ground_travel(caller, world, movement, &attribute, owns, claim);
            return;
        }
        let gravity = attribute(&Attributes::GRAVITY);
        let modifier = attribute(&Attributes::AIR_DRAG_MODIFIER) as f32;
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

    fn try_apply_ground_travel(
        &self,
        caller: &dyn EntityBase,
        world: &World,
        movement: Vector3<f64>,
        attribute: &impl Fn(&Attributes) -> f64,
        owns: &impl Fn(Vector2<i32>) -> bool,
        claim: impl FnOnce() -> bool,
    ) {
        if movement.horizontal_length_squared() == 0.0 || self.fall_distance.load() != 0.0 {
            return;
        }
        let Ok(equipment) = self.entity_equipment.try_lock() else {
            return;
        };
        if !equipment.get(&EquipmentSlot::FEET).is_empty() {
            return;
        }

        let entity = &self.entity;
        let origin = entity.pos.load();
        let bounds = entity.bounding_box.load();
        let sweep = bounds.stretch(movement);
        let min = BlockPos::floored_v(sweep.min.add_raw(0.0, -COLLISION_LOOK_BELOW, 0.0));
        let max = sweep.max_block_pos();
        world.level.try_with_regional_path(
            min,
            max,
            owns,
            |read| {
                let (supporting, floor, movement) = continuous_ground_collision(
                    world,
                    bounds,
                    movement,
                    min,
                    max,
                    entity.supporting_block_pos.load(),
                    read,
                )?;
                let position = origin + movement;
                let next = Entity::make_bounding_box(
                    entity.entity_type,
                    position,
                    &entity.entity_dimension.load(),
                );
                let border = world.worldborder.try_lock().ok()?;
                if !border.contains(next.min.x, next.min.z)
                    || !border.contains(next.max.x, next.max.z)
                {
                    return None;
                }
                let emission = if caller.movement_emission().emits_anything() {
                    let (walked, flown) = entity.movement_distances(movement, false);
                    if walked > entity.next_step.load() {
                        return None;
                    }
                    Some((walked, flown))
                } else {
                    None
                };
                let friction = Self::compute_modified_friction(
                    floor.to_block().slipperiness,
                    attribute(&Attributes::FRICTION_MODIFIER) as f32,
                );
                let (velocity, _) = self.apply_air_drag(
                    caller,
                    movement.add_raw(0.0, -attribute(&Attributes::GRAVITY), 0.0),
                    attribute(&Attributes::AIR_DRAG_MODIFIER) as f32,
                    friction,
                );
                velocity.length_squared().is_finite().then_some((
                    BlockPos::floored_v(position),
                    (position, next, velocity, supporting, emission, border),
                ))
            },
            |(position, next, velocity, supporting, emission, border), biome| {
                let vicinity = sweep.expand(1.0, 1.0, 1.0);
                world.entity_index.try_update_isolated_actor_bounds(
                    world,
                    entity.entity_id,
                    vicinity,
                    vicinity,
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
                        entity.vertical_collision.store(true, Relaxed);
                        entity.supporting_block_pos.store(Some(supporting));
                        entity.velocity.store(velocity);
                        if let Some((walked, flown)) = emission {
                            entity.move_dist.store(walked);
                            entity.fly_dist.store(flown);
                        }
                        true
                    },
                );
                drop(border);
            },
        );
        drop(equipment);
    }
}

fn continuous_ground_collision(
    world: &World,
    bounds: BoundingBox,
    movement: Vector3<f64>,
    min: BlockPos,
    max: BlockPos,
    previous_support: Option<BlockPos>,
    read: &dyn Fn(BlockPos) -> Option<BlockStateId>,
) -> Option<(BlockPos, BlockStateId, Vector3<f64>)> {
    let collision = regional_floor_collision(world, bounds, movement, min, max, read)?;
    let supporting = collision.supporting?;
    if collision.horizontal || collision.movement.y != 0.0 || !collision.vertical {
        return None;
    }
    let floor = read(supporting)?;
    let min = BlockPos::new(min.0.x, supporting.0.y, min.0.z);
    let max = BlockPos::new(max.0.x, supporting.0.y, max.0.z);
    (BlockPos::iterate(min, max).all(|pos| read(pos) == Some(floor))
        && previous_support.is_none_or(|pos| read(pos) == Some(floor)))
    .then_some((supporting, floor, collision.movement))
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use pumpkin_data::Block;
    use pumpkin_world::chunk::ChunkData;

    use super::*;
    use crate::entity::{ai::test_support, passive::cow::CowEntity};

    #[tokio::test]
    async fn grounded_regional_travel_matches_ordinary_travel_and_yields_at_an_edge()
    -> Result<(), Box<dyn std::error::Error>> {
        let regional_folder = tempfile::tempdir()?;
        let ordinary_folder = tempfile::tempdir()?;
        let regional = test_support::world(&regional_folder);
        let ordinary = test_support::world(&ordinary_folder);
        let chunk_pos = Vector2::new(0, 0);
        let origin = Vector3::new(10.5, 64.0, 7.5);
        let velocity = Vector3::new(0.55, -0.08, 0.0);
        let prepare = |world: &Arc<World>| {
            let chunk = ChunkData::empty_sync(0, 0);
            for x in 6..=13 {
                for z in 5..=10 {
                    chunk.set_block_absolute_y(x, 63, z, Block::STONE.default_state.id);
                }
            }
            world.level.loaded_chunks.insert(chunk_pos, chunk.clone());
            let cow = CowEntity::new(Entity::new(world.clone(), origin, &EntityType::COW));
            let base = cow.get_entity();
            base.on_ground.store(true, Relaxed);
            base.first_tick.store(false, Relaxed);
            base.velocity.store(velocity);
            world.add_entity_silent(cow.clone());
            (cow, chunk)
        };
        let (cow, chunk) = prepare(&regional);
        let (comparison, _) = prepare(&ordinary);
        let living = cow
            .get_living_entity()
            .ok_or("missing regional living entity")?;
        let regular = comparison
            .get_living_entity()
            .ok_or("missing ordinary living entity")?;
        let now = Instant::now();
        let period = Duration::from_millis(50);
        let step = |at| TickDeadline { at, period };
        let owns = |pos| pos == chunk_pos;
        living.try_regional_travel(cow.as_ref(), &regional, 0, step(now), &owns, || ());
        regular.travel(comparison.as_ref());
        let position = cow.get_entity().pos.load();
        assert!(position.x > origin.x);
        assert!((position - comparison.get_entity().pos.load()).length() < 1.0e-8);
        assert!(
            (cow.get_entity().velocity.load() - comparison.get_entity().velocity.load()).length()
                < 1.0e-8
        );
        assert!(cow.get_entity().on_ground.load(Relaxed));
        assert!(cow.get_entity().vertical_collision.load(Relaxed));

        chunk.set_block_absolute_y(11, 63, 7, Block::AIR.default_state.id);
        living.try_regional_travel(cow.as_ref(), &regional, 0, step(now + period), &owns, || ());
        assert_eq!(cow.get_entity().pos.load(), position);
        regional.level.shutdown().await;
        ordinary.level.shutdown().await;
        Ok(())
    }
}
