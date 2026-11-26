use std::sync::atomic::Ordering::{Acquire, Relaxed};

use pumpkin_data::{
    BlockStateId,
    attributes::Attributes,
    entity::EntityType,
    game_event::GameEvent,
    tag::{self, Taggable},
};
use sculith_util::math::{
    boundingbox::BoundingBox, position::BlockPos, vector2::Vector2, vector3::Vector3,
};

use crate::{
    entity::{
        Entity, EntityBase,
        ai::{
            control::move_control::{MoveControl, Operation},
            pathfinder::{GroundRouteStep, PathNavigation},
        },
        drop_motion::{ActorTickOwner, regional_floor_collision},
        mob::Mob,
    },
    plugin::api::events::world::generic_game::GenericGameEvent,
    server::TickDeadline,
    world::{COLLISION_LOOK_BELOW, IsolatedMovement, RegionId, World, game_event::notified_area},
};

use super::{EquipmentSlot, LivingEntity};

pub fn supports_regional_travel(kind: &EntityType) -> bool {
    kind == &EntityType::COW || kind == &EntityType::PIG || kind == &EntityType::SHEEP
}

struct GroundSteering<'a> {
    navigator: &'a mut PathNavigation,
    control: &'a mut MoveControl,
    route: GroundRouteStep,
    yaw: f32,
    speed: f32,
    footing: BlockPos,
    target_floor: BlockPos,
    floor: BlockStateId,
}

struct GroundTravel<'a> {
    movement: Vector3<f64>,
    steering: Option<GroundSteering<'a>>,
}

struct GroundStep {
    supporting: BlockPos,
    floor: BlockStateId,
    sound: bool,
    listeners: BoundingBox,
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
                    (owns(chunk) && clock.claim(ActorTickOwner::Region(owner), deadline))
                        .then_some(deadline)
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
        claim: impl FnOnce() -> Option<TickDeadline>,
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
        let on_ground = entity.on_ground.load(Relaxed);
        // Bound the span of the palette sweep for large external impulses.
        if !(-2.0..=-0.003).contains(&movement.y)
            || !(0.0..=1.0).contains(&movement.horizontal_length_squared())
            || !on_ground && self.movement_input.load() != Vector3::default()
        {
            return;
        }
        // These mobs use the standard gravity attribute. Keep its value and
        // drag stable through the step without waiting for a world edit.
        let Ok(attributes) = self.attributes.try_read() else {
            return;
        };
        let attribute = |kind: &Attributes| Self::attribute_value_from(&attributes, kind);
        if on_ground {
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
                        if claim().is_none() {
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
        claim: impl FnOnce() -> Option<TickDeadline>,
    ) {
        if self.fall_distance.load() != 0.0 {
            return;
        }
        let Ok(equipment) = self.entity_equipment.try_lock() else {
            return;
        };
        if !equipment.get(&EquipmentSlot::FEET).is_empty() {
            return;
        }
        let input = self.movement_input.load();
        if input == Vector3::default() {
            let travel = GroundTravel {
                movement,
                steering: None,
            };
            self.try_commit_ground_travel(caller, world, travel, attribute, owns, claim);
            return;
        }
        if input.x != 0.0 || input.y != 0.0 || !(input.z > 0.0 && input.z.is_finite()) {
            return;
        }
        let Some(mob) = caller.get_mob().map(Mob::get_mob_entity) else {
            return;
        };
        let Some(mut navigator) = mob.try_navigator_for_regional_motion() else {
            return;
        };
        let Ok(mut control) = mob.move_control.try_lock() else {
            return;
        };
        let (Some(navigator), Some(control)) =
            (navigator.ground_navigation(), control.idle_ground_control())
        else {
            return;
        };
        let Some(travel) =
            prepare_ground_steering(self, world, navigator, control, movement, attribute)
        else {
            return;
        };
        self.try_commit_ground_travel(caller, world, travel, attribute, owns, claim);
        drop(equipment);
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Validates steering, palettes and movement as one commit"
    )]
    fn try_commit_ground_travel(
        &self,
        caller: &dyn EntityBase,
        world: &World,
        travel: GroundTravel<'_>,
        attribute: &impl Fn(&Attributes) -> f64,
        owns: &impl Fn(Vector2<i32>) -> bool,
        claim: impl FnOnce() -> Option<TickDeadline>,
    ) {
        if !(0.0 < travel.movement.horizontal_length_squared()
            && travel.movement.horizontal_length_squared() <= 1.0)
        {
            return;
        }
        let entity = &self.entity;
        let GroundTravel { movement, steering } = travel;
        let origin = entity.pos.load();
        let bounds = entity.bounding_box.load();
        let sweep = bounds.stretch(movement);
        let min = BlockPos::floored_v(sweep.min.add_raw(0.0, -COLLISION_LOOK_BELOW, 0.0));
        let max = sweep.max_block_pos();
        let route_floor = steering.as_ref().map(|plan| {
            (
                plan.footing,
                plan.target_floor,
                plan.floor,
                plan.route.terrain_revision,
            )
        });
        let (read_min, read_max) = route_floor.map_or((min, max), |(footing, target, ..)| {
            extend_read_extent(min, max, [footing, target])
        });
        let mut step_sound = None;
        world.level.try_with_regional_path(
            read_min,
            read_max,
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
                if route_floor.is_some_and(|(footing, target, expected, _)| {
                    floor != expected
                        || read(footing) != Some(expected)
                        || read(target) != Some(expected)
                }) {
                    return None;
                }
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
                let emission = caller.movement_emission();
                let distances = emission
                    .emits_anything()
                    .then(|| entity.movement_distances(movement, false));
                let step = if distances.is_some_and(|(walked, _)| walked > entity.next_step.load())
                {
                    if entity.is_swimming()
                        || floor
                            .to_block()
                            .has_tag(&tag::Block::MINECRAFT_CRYSTAL_SOUND_BLOCKS)
                        || world.server.upgrade().is_some_and(|server| {
                            server.plugin_manager.has_handlers::<GenericGameEvent>()
                        })
                    {
                        return None;
                    }
                    Some(GroundStep {
                        supporting,
                        floor,
                        sound: emission.emits_sounds(),
                        listeners: notified_area(position, GameEvent::Step.notification_radius()),
                    })
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
                    (
                        position, next, velocity, supporting, distances, step, border,
                    ),
                ))
            },
            |(position, next, velocity, supporting, distances, step, border), biome| {
                let vicinity = sweep.expand(1.0, 1.0, 1.0);
                world.entity_index.try_apply_isolated_movement(
                    world,
                    entity.entity_id,
                    &IsolatedMovement {
                        swept: vicinity,
                        player_clearance: vicinity,
                        bounds: next,
                        listener_clearance: step.as_ref().map(|step| step.listeners),
                    },
                    || {
                        if route_floor.is_some_and(|(.., revision)| {
                            world.navigation_change_sequence.load(Acquire) != revision
                        }) {
                            return false;
                        }
                        let Some(deadline) = claim() else {
                            return false;
                        };
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
                        if let Some(plan) = steering {
                            let target = &plan.route.target;
                            plan.control.set_wanted_position(
                                target.position.x,
                                target.position.y,
                                target.position.z,
                                target.speed_modifier,
                            );
                            plan.control.operation = Operation::Wait;
                            entity.yaw.store(plan.yaw);
                            self.set_speed(plan.speed);
                            self.movement_input.store(Vector3::new(
                                0.0,
                                0.0,
                                f64::from(plan.speed),
                            ));
                            plan.navigator.commit_ground_route(&plan.route, deadline);
                        }
                        if let Some((walked, flown)) = distances {
                            entity.move_dist.store(walked);
                            entity.fly_dist.store(flown);
                        }
                        if let Some(step) = step {
                            entity.next_step.store(caller.next_step());
                            if step.sound {
                                step_sound = Some((step.supporting, step.floor));
                            }
                        }
                        true
                    },
                );
                drop(border);
            },
        );
        if let Some((supporting, floor)) = step_sound {
            // Publish after releasing terrain and index locks, while the actor is still owned.
            caller.play_step_sound(&supporting, floor.to_state());
        }
    }
}

fn prepare_ground_steering<'a>(
    living: &LivingEntity,
    world: &World,
    navigator: &'a mut PathNavigation,
    control: &'a mut MoveControl,
    original_movement: Vector3<f64>,
    attribute: &impl Fn(&Attributes) -> f64,
) -> Option<GroundTravel<'a>> {
    let world_age = world.level_time.try_lock().ok()?.world_age as u64;
    let route = navigator.prepare_ground_route(living, world, world_age)?;
    let entity = &living.entity;
    let delta = route.target.position - entity.pos.load();
    if delta.y != 0.0 || delta.horizontal_length_squared() < f64::from(MoveControl::MIN_SPEED_SQR) {
        return None;
    }
    let yaw = MoveControl::turn_toward(entity.yaw.load(), delta.x, delta.z);
    let speed = (route.target.speed_modifier * attribute(&Attributes::MOVEMENT_SPEED)) as f32;
    if !(speed > 0.0 && speed.is_finite()) {
        return None;
    }
    let footing = entity
        .supporting_block_pos
        .load()
        .unwrap_or_else(|| entity.block_pos.load().down());
    let floor = world.level.try_read_block_state(footing)?;
    let friction = LivingEntity::compute_modified_friction(
        floor.to_block().slipperiness,
        attribute(&Attributes::FRICTION_MODIFIER) as f32,
    );
    let friction_speed = LivingEntity::friction_influenced_speed(f64::from(speed), friction);
    let movement = original_movement
        + Entity::relative_input_velocity(
            yaw,
            Vector3::new(0.0, 0.0, f64::from(speed)),
            friction_speed,
        );
    let target_floor = BlockPos::floored_v(route.target.position).down();
    Some(GroundTravel {
        movement,
        steering: Some(GroundSteering {
            navigator,
            control,
            route,
            yaw,
            speed,
            footing,
            target_floor,
            floor,
        }),
    })
}

fn extend_read_extent(
    mut min: BlockPos,
    mut max: BlockPos,
    positions: [BlockPos; 2],
) -> (BlockPos, BlockPos) {
    for position in positions {
        min.0.x = min.0.x.min(position.0.x);
        min.0.z = min.0.z.min(position.0.z);
        max.0.x = max.0.x.max(position.0.x);
        max.0.z = max.0.z.max(position.0.z);
    }
    (min, max)
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
    use sculith_world::{chunk::ChunkData, level::SyncChunk};

    use super::*;
    use crate::entity::{
        ai::{
            pathfinder::{node::Node, path::Path},
            test_support,
        },
        passive::{allay::AllayEntity, cow::CowEntity},
    };

    fn grounded_cow(
        world: &Arc<World>,
        origin: Vector3<f64>,
        velocity: Vector3<f64>,
    ) -> (Arc<CowEntity>, SyncChunk) {
        let chunk = ChunkData::empty_sync(0, 0);
        for x in 6..=13 {
            for z in 5..=10 {
                chunk.set_block_absolute_y(x, 63, z, Block::STONE.default_state.id);
            }
        }
        world
            .level
            .loaded_chunks
            .insert(Vector2::new(0, 0), chunk.clone());
        let cow = CowEntity::new(Entity::new(world.clone(), origin, &EntityType::COW));
        let base = cow.get_entity();
        base.on_ground.store(true, Relaxed);
        base.first_tick.store(false, Relaxed);
        base.velocity.store(velocity);
        world.add_entity_silent(cow.clone());
        (cow, chunk)
    }

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
        let (cow, chunk) = grounded_cow(&regional, origin, velocity);
        let (comparison, _) = grounded_cow(&ordinary, origin, velocity);
        cow.get_entity().next_step.store(0.1);
        comparison.get_entity().next_step.store(0.1);
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
        assert!(cow.get_entity().next_step.load() > 0.1);
        assert!(
            (cow.get_entity().next_step.load() - comparison.get_entity().next_step.load()).abs()
                < f32::EPSILON
        );

        chunk.set_block_absolute_y(11, 63, 7, Block::AIR.default_state.id);
        living.try_regional_travel(cow.as_ref(), &regional, 0, step(now + period), &owns, || ());
        assert_eq!(cow.get_entity().pos.load(), position);
        regional.level.shutdown().await;
        ordinary.level.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn regional_footstep_yields_to_a_game_event_listener_outside_the_collision_sweep()
    -> Result<(), Box<dyn std::error::Error>> {
        let folder = tempfile::tempdir()?;
        let world = test_support::world(&folder);
        let origin = Vector3::new(10.5, 64.0, 7.5);
        let (cow, _) = grounded_cow(&world, origin, Vector3::new(0.55, -0.08, 0.0));
        let listener = AllayEntity::new(Entity::new(
            world.clone(),
            Vector3::new(14.5, 64.0, 7.5),
            &EntityType::ALLAY,
        ));
        world.add_entity_silent(listener);
        cow.get_entity().next_step.store(0.1);
        let deadline = TickDeadline {
            at: Instant::now(),
            period: Duration::from_millis(50),
        };
        cow.get_living_entity()
            .ok_or("missing regional living entity")?
            .try_regional_travel(
                cow.as_ref(),
                &world,
                0,
                deadline,
                &|pos| pos == Vector2::new(0, 0),
                || (),
            );
        assert_eq!(cow.get_entity().pos.load(), origin);
        assert!((cow.get_entity().next_step.load() - 0.1).abs() < f32::EPSILON);
        world.level.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn regional_ground_route_matches_the_world_controller_and_records_the_deadline()
    -> Result<(), Box<dyn std::error::Error>> {
        let regional_folder = tempfile::tempdir()?;
        let ordinary_folder = tempfile::tempdir()?;
        let regional = test_support::world(&regional_folder);
        let ordinary = test_support::world(&ordinary_folder);
        let origin = Vector3::new(10.5, 64.0, 7.5);
        let velocity = Vector3::new(0.0, -0.08, 0.0);
        let (cow, _) = grounded_cow(&regional, origin, velocity);
        let (comparison, _) = grounded_cow(&ordinary, origin, velocity);
        let goal = BlockPos::new(12, 64, 7);
        for actor in [&cow, &comparison] {
            let mob = actor.get_mob_entity();
            mob.clear_ai_goals(actor.as_ref());
            let nodes = (10..=12)
                .map(|x| Node::new(BlockPos::new(x, 64, 7)))
                .collect();
            assert!(mob.steered().navigator().move_to_path(
                Some(Path::new(nodes, goal, true)),
                1.0,
                &mob.living_entity,
            ));
            mob.set_speed(0.2);
        }

        let deadline = TickDeadline {
            at: Instant::now(),
            period: Duration::from_millis(50),
        };
        let mob = cow.get_mob_entity();
        let regular_mob = comparison.get_mob_entity();
        let regional_step = |step| {
            mob.living_entity.try_regional_travel(
                cow.as_ref(),
                &regional,
                0,
                step,
                &|chunk| chunk == Vector2::new(0, 0),
                || (),
            );
        };
        mob.steered().move_control().strafe(0.6, 0.1);
        regional_step(deadline);
        assert_eq!(cow.get_entity().pos.load(), origin);
        *mob.steered().move_control() = Box::new(MoveControl::default());
        regional_step(deadline);
        regular_mob.server_ai_step(comparison.as_ref(), comparison.as_ref());
        regular_mob.living_entity.travel(comparison.as_ref());

        let actor = cow.get_entity();
        let expected = comparison.get_entity();
        let advanced = actor.pos.load();
        assert_ne!(advanced, origin);
        assert!((advanced - expected.pos.load()).length() < 1.0e-8);
        assert!((actor.velocity.load() - expected.velocity.load()).length() < 1.0e-8);
        assert!((actor.yaw.load() - expected.yaw.load()).abs() < f32::EPSILON);
        let deadline_after = TickDeadline {
            at: deadline.at + deadline.period,
            ..deadline
        };
        let regular_next = regular_mob
            .steered()
            .navigator()
            .get_path()
            .ok_or("missing world path")?
            .get_next_node_index();
        let (regional_next, advanced_on_deadline, advanced_on_next) = {
            let steered = mob.steered();
            let mut navigator = steered.navigator();
            let next = navigator
                .get_path()
                .ok_or("missing regional path")?
                .get_next_node_index();
            let ground = navigator
                .ground_navigation()
                .ok_or("missing ground navigator")?;
            (
                next,
                ground.already_followed(Some(deadline)),
                ground.already_followed(Some(deadline_after)),
            )
        };
        assert_eq!(regional_next, regular_next);
        assert!(advanced_on_deadline);
        assert!(!advanced_on_next);
        regional_step(deadline);
        assert_eq!(actor.pos.load(), advanced);
        regional
            .navigation_change_sequence
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        regional_step(deadline_after);
        assert_eq!(actor.pos.load(), advanced);
        regional.level.shutdown().await;
        ordinary.level.shutdown().await;
        Ok(())
    }
}
