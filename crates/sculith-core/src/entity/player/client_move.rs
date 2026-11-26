use std::sync::Arc;
use std::sync::atomic::Ordering;

use dashmap::try_result::TryResult;
use pumpkin_data::data_component_impl::{EnchantmentsImpl, EquipmentSlot};
use pumpkin_data::{Block, BlockStateId, entity::EntityPose, tag::Taggable, translation};
use rustc_hash::FxHashSet;
use sculith_protocol::bedrock::client::CMovePlayer;
use sculith_protocol::java::client::play::{
    CEntityPositionSync, CHeadRot, CPlayerPosition, CSetCamera, CUpdateEntityPos,
    CUpdateEntityPosRot, CUpdateEntityRot,
};
use sculith_util::GameMode;
use sculith_util::math::{position::BlockPos, vector3::Vector3, wrap_degrees};
use sculith_util::text::TextComponent;

use crate::entity::{Entity, EntityBase};
use crate::net::{ClientPlatform, DisconnectReason};
use crate::plugin::entity::entity_exhaustion::EntityExhaustionEvent;
use crate::plugin::player::player_input::PlayerInputEvent;
use crate::plugin::player::player_move::PlayerMoveEvent;
use crate::plugin::player::player_statistic_increment::PlayerStatisticIncrementEvent;
use crate::plugin::player::player_toggle_flight_event::PlayerToggleFlightEvent;
use crate::plugin::player::player_toggle_sneak_event::PlayerToggleSneakEvent;
use crate::plugin::player::player_toggle_sprint_event::PlayerToggleSprintEvent;
use crate::plugin::server::packet::PacketSentEvent;
use crate::server::Server;
use crate::world::{RegionChunks, World, chunker};

use super::Player;
use super::statistics::{CustomStatistic, StatisticCategory};

bitflags::bitflags! {
    /// Player controls with the established native plugin and input event encoding.
    #[derive(Clone, Copy)]
    pub(crate) struct PlayerInput: i8 {
        const FORWARD = 1 << 0;
        const BACKWARD = 1 << 1;
        const LEFT = 1 << 2;
        const RIGHT = 1 << 3;
        const JUMP = 1 << 4;
        const SNEAK = 1 << 5;
        const SPRINT = 1 << 6;
    }
}

/// One client move this tick, from Java `ServerboundMovePlayerPacket` or Bedrock `PlayerAuthInput`.
#[derive(Clone, Copy)]
pub struct ClientMove {
    pub position: Option<Vector3<f64>>,
    pub yaw: Option<f32>,
    pub pitch: Option<f32>,
    pub head_yaw: Option<f32>,
    pub on_ground: bool,
    pub horizontal_collision: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientMoveOutcome {
    Applied,
    Rejected,
    Ignored,
}

#[derive(Clone, Copy)]
pub(crate) struct RegionalMoveStamp {
    world: uuid::Uuid,
    teleport: i32,
    epoch: u32,
    position: Vector3<f64>,
}

impl RegionalMoveStamp {
    pub(crate) fn is_current(self, player: &Player, world: &World) -> bool {
        world.uuid == self.world
            && player.can_simulate_in(world)
            && player.teleport_id_count.load(Ordering::Relaxed) == self.teleport
            && player.chunk_send_epoch.load(Ordering::Relaxed) == self.epoch
            && player.get_entity().pos.load() == self.position
    }
}

pub(crate) struct CommittedPlayerMove {
    pub(crate) stamp: RegionalMoveStamp,
    from: Vector3<f64>,
    movement: ClientMove,
    rotation_changed: bool,
    received: FxHashSet<i32>,
}

pub(crate) enum RegionalClientMove {
    Complete {
        position_changed: bool,
        rotation_changed: bool,
        stamp: RegionalMoveStamp,
    },
    Committed(CommittedPlayerMove),
}

#[derive(Clone, Copy)]
struct RegionalPose {
    rotation_changed: bool,
    stationary_settled: bool,
    grounded_move: bool,
}

#[derive(Clone, Copy)]
enum MovePublication<'a> {
    Delta,
    Committed(&'a CommittedPlayerMove),
}

impl Player {
    const fn clamp_horizontal(pos: f64) -> f64 {
        pos.clamp(-3.0E7, 3.0E7)
    }

    const fn clamp_vertical(pos: f64) -> f64 {
        pos.clamp(-2.0E7, 2.0E7)
    }

    fn packed_rot(degrees: f32) -> u8 {
        (degrees * 256.0 / 360.0).rem_euclid(256.0) as u8
    }

    /// Applies the input mask exposed by the native plugin API.
    pub fn apply_client_input(&self, server: &Arc<Server>, input: i8) {
        self.apply_player_input(server, PlayerInput::from_bits_retain(input));
    }

    pub(crate) fn player_input(&self) -> PlayerInput {
        PlayerInput::from_bits_retain(self.last_input.load(Ordering::Relaxed))
    }

    pub(crate) fn apply_player_input(&self, server: &Arc<Server>, input: PlayerInput) {
        let Some(player) = self.world().get_player_by_uuid(self.gameprofile.id) else {
            return;
        };
        let mut input_event = PlayerInputEvent::new(player.clone(), format!("{:b}", input.bits()));
        server
            .plugin_manager
            .fire_blocking(server, &mut input_event);
        if input_event.cancelled {
            return;
        }

        self.last_input.store(input.bits(), Ordering::Relaxed);

        let sneak = input.contains(PlayerInput::SNEAK);
        if sneak
            && self.gamemode.load() == GameMode::Spectator
            && self.camera_target_id.load().is_some()
        {
            self.camera_target_id.store(None);
            self.try_send_client_packet(&CSetCamera::new(self.entity_id().into()));
        }

        let entity = self.get_entity();
        if entity.is_sneaking() != sneak {
            send_cancellable_blocking! {{
                server;
                PlayerToggleSneakEvent::new(player, sneak);
                'after: {
                    entity.set_sneaking(event.is_sneaking);
                    if event.is_sneaking {
                        dismount_if_riding(entity);
                    }
                }
            }}
        } else if sneak {
            dismount_if_riding(entity);
        }
    }

    /// Vanilla `ServerboundPlayerCommandPacket` start/stop sprinting
    pub fn apply_sprint_input(&self, server: &Arc<Server>, sprinting: bool) {
        if self.get_entity().is_sprinting() == sprinting {
            return;
        }
        let Some(player) = self.world().get_player_by_uuid(self.gameprofile.id) else {
            return;
        };
        send_cancellable_blocking! {{
            server;
            PlayerToggleSprintEvent::new(player, sprinting);
            'after: {
                self.set_sprinting(event.is_sprinting);
                self.update_player_pose();
            }
        }}
    }

    /// Vanilla `ServerboundPlayerAbilitiesPacket` flying bit
    pub fn apply_flight_input(&self, server: &Arc<Server>, flying: bool) {
        let (is_flying, allow_flying) = {
            let abilities = self
                .abilities
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (abilities.flying, abilities.allow_flying)
        };
        let new_flying = flying && allow_flying;
        if is_flying == new_flying {
            return;
        }
        let Some(player) = self.world().get_player_by_uuid(self.gameprofile.id) else {
            return;
        };
        send_cancellable_blocking! {{
            server;
            PlayerToggleFlightEvent::new(player, new_flying);
            'after: {
                if event.is_flying {
                    self.living_entity.fall_distance.store(0.0);
                }
                self.abilities.lock().unwrap_or_else(std::sync::PoisonError::into_inner).flying = event.is_flying;
                self.send_abilities_update();
            }
            'cancelled: {
                self.send_abilities_update();
            }
        }}
    }

    /// Bedrock can stop a glide in the air or start one directly from creative flight.
    pub(crate) fn apply_bedrock_glide_input(&self, server: &Arc<Server>, gliding: bool) {
        let entity = self.get_entity();
        if entity.is_fall_flying() == gliding {
            return;
        }
        if !gliding {
            if !self.try_to_stop_fall_flying() {
                // Correct the client's predicted stop after a plugin veto.
                entity.set_fall_flying(true);
            }
            return;
        }

        let was_flying = self.is_flying();
        let set_flying = |flying| {
            self.abilities
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .flying = flying;
        };
        if was_flying {
            set_flying(false);
        }
        if !self.try_to_start_fall_flying(server) {
            if was_flying {
                set_flying(true);
            }
            entity.stop_fall_flying();
        }
        if was_flying {
            self.send_abilities_update();
        }
    }

    /// Snap the client back. Vanilla `ServerGamePacketListenerImpl.teleport` used when a move is refused.
    pub fn reject_client_move(&self) {
        let entity = self.get_entity();
        let position = entity.pos.load();
        let teleport_id = self.teleport_id_count.fetch_add(1, Ordering::Relaxed) + 1;
        if !matches!(
            self.client.as_ref(),
            ClientPlatform::Java(c)
                if c.version.load() < sculith_util::version::JavaMinecraftVersion::V_1_9
        ) {
            *self
                .awaiting_teleport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some((teleport_id.into(), position));
        }
        match self.client.as_ref() {
            ClientPlatform::Java(_) => {
                self.try_send_client_packet(&CPlayerPosition::new(
                    teleport_id.into(),
                    position,
                    Vector3::new(0.0, 0.0, 0.0),
                    entity.yaw.load(),
                    entity.pitch.load(),
                    Vec::new(),
                ));
            }
            ClientPlatform::Bedrock(client) => {
                self.send_bedrock_teleport(client);
            }
        }
    }

    pub(crate) fn controlled_vehicle(&self) -> Option<Arc<dyn EntityBase>> {
        let vehicle = self.get_entity().get_vehicle()?;
        let base = vehicle.get_entity();
        if base.is_removed()
            || base.has_vehicle()
            || !Arc::ptr_eq(&base.world.load(), &self.world())
            || !self.has_client_loaded()
            || self.is_movement_locked()
            || self.living_entity.is_dead_or_dying()
            || self
                .awaiting_teleport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some()
        {
            return None;
        }
        vehicle
            .get_controlling_passenger()
            .is_some_and(|rider| rider.get_entity().entity_id == self.entity_id())
            .then_some(vehicle)
    }

    /// Applies the controlling rider's accepted movement to the shared vehicle and its riders.
    pub(crate) fn apply_client_vehicle_move(
        self: &Arc<Self>,
        vehicle: &dyn EntityBase,
        position: Vector3<f64>,
        yaw: f32,
        pitch: f32,
        on_ground: bool,
    ) {
        if ![position.x, position.y, position.z]
            .into_iter()
            .all(f64::is_finite)
            || !yaw.is_finite()
            || !pitch.is_finite()
        {
            self.kick_invalid_movement();
            return;
        }
        let position = Vector3::new(
            Self::clamp_horizontal(position.x),
            Self::clamp_vertical(position.y),
            Self::clamp_horizontal(position.z),
        );
        let previous = self.get_entity().pos.load();
        let base = vehicle.get_entity();
        let motion = position - base.pos.load();
        base.set_pos(position);
        base.set_rotation(wrap_degrees(yaw), wrap_degrees(pitch));
        base.on_ground.store(on_ground, Ordering::Relaxed);
        base.emit_client_movement(vehicle, motion);
        if let Some(living) = vehicle.get_living_entity() {
            living.fall(vehicle, motion.y, on_ground);
        }

        self.update_vehicle_lava_advancement(base, position);
        crate::entity::position_riders(vehicle);
        let cm = (previous
            .squared_distance_to_vec(&self.get_entity().pos.load())
            .sqrt()
            * 100.0)
            .round() as i32;
        if cm > 0 {
            let stat = self.get_movement_statistic();
            self.increment_stat(StatisticCategory::Custom, stat as i32, cm);
        }
        chunker::update_position(self);
    }

    fn update_vehicle_lava_advancement(&self, vehicle: &crate::entity::Entity, pos: Vector3<f64>) {
        let world = self.world();
        let block = pos.to_block_pos();
        let below = BlockPos::new(block.0.x, block.0.y - 1, block.0.z);
        if world.get_block(&block) == &Block::LAVA || world.get_block(&below) == &Block::LAVA {
            let start = self.entered_lava_pos.load().unwrap_or(pos);
            self.entered_lava_pos.store(Some(start));
            self.trigger_advancement(
                super::advancement::trigger::AdvancementTrigger::RideEntityInLava {
                    distance_horizontal: (pos.x - start.x).hypot(pos.z - start.z),
                    dimension: world.dimension.minecraft_name.to_string(),
                    vehicle_type: Some(format!("minecraft:{}", vehicle.entity_type.resource_name)),
                },
            );
        } else {
            self.entered_lava_pos.store(None);
        }
    }

    /// Vanilla `handlePlayerPositionChange`. Does not write `Entity.velocity`; that stays server
    /// owned (`deltaMovement`). Client motion this tick is `Entity.movement`.
    pub fn apply_client_move(
        &self,
        server: &Arc<Server>,
        requested: ClientMove,
    ) -> ClientMoveOutcome {
        self.apply_client_move_with_extent(server, requested, None)
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn apply_client_move_with_extent(
        &self,
        server: &Arc<Server>,
        requested: ClientMove,
        extent: Option<&RegionChunks>,
    ) -> ClientMoveOutcome {
        if !self.has_client_loaded() {
            return ClientMoveOutcome::Ignored;
        }

        if requested
            .position
            .is_some_and(|p| !(p.x.is_finite() && p.y.is_finite() && p.z.is_finite()))
            || requested.yaw.is_some_and(|yaw| !yaw.is_finite())
            || requested.pitch.is_some_and(|pitch| !pitch.is_finite())
            || requested.head_yaw.is_some_and(|yaw| !yaw.is_finite())
        {
            self.kick_invalid_movement();
            return ClientMoveOutcome::Rejected;
        }

        // Vanilla `absSnapRotationTo` while awaiting a teleport: look only.
        if self
            .awaiting_teleport
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
        {
            self.apply_and_broadcast_look(requested);
            return ClientMoveOutcome::Ignored;
        }

        let entity = self.get_entity();
        let last_pos = entity.pos.load();
        let mut target = requested.position.map(|pos| {
            Vector3::new(
                Self::clamp_horizontal(pos.x),
                Self::clamp_vertical(pos.y),
                Self::clamp_horizontal(pos.z),
            )
        });

        if self.is_movement_locked.load(Ordering::Relaxed) {
            self.apply_and_broadcast_look(requested);
            self.reject_client_move();
            return ClientMoveOutcome::Rejected;
        }

        if entity.has_vehicle() {
            // Vanilla passenger: rotation only, keep the vehicle's position.
            entity.on_ground.store(false, Ordering::Relaxed);
            entity.horizontal_collision.store(false, Ordering::Relaxed);
            self.apply_and_broadcast_look(requested);
            entity.movement.store(Vector3::default());
            return ClientMoveOutcome::Applied;
        }

        if self.sleeping_since.load().is_some()
            && let Some(pos) = target
            && (pos - last_pos).length_squared() > 1.0
        {
            self.reject_client_move();
            return ClientMoveOutcome::Rejected;
        }

        if let Some(pos) = target
            && pos != last_pos
            && server.plugin_manager.has_handlers::<PlayerMoveEvent>()
        {
            let Some(player) = self.world().get_player_by_uuid(self.gameprofile.id) else {
                return ClientMoveOutcome::Ignored;
            };
            let mut cancelled = false;
            send_cancellable_blocking! {{
                server;
                PlayerMoveEvent::new(player, last_pos, pos);
                'after: {
                    target = Some(event.to);
                }
                'cancelled: {
                    cancelled = true;
                }
            }}
            if cancelled {
                self.reject_client_move();
                return ClientMoveOutcome::Rejected;
            }
        }

        let pos = target.unwrap_or(last_pos);
        let was_on_ground = entity.on_ground.load(Ordering::Relaxed);
        if !extent.is_some_and(|extent| {
            self.try_update_regional_pose(
                server,
                extent,
                last_pos,
                pos,
                None,
                None::<fn() -> Option<()>>,
            )
            .is_some()
        }) {
            entity.set_pos(pos);
        }
        self.finish_client_move(
            last_pos,
            pos,
            requested,
            was_on_ground,
            MovePublication::Delta,
        );
        ClientMoveOutcome::Applied
    }

    fn finish_client_move(
        &self,
        last_pos: Vector3<f64>,
        pos: Vector3<f64>,
        requested: ClientMove,
        was_on_ground: bool,
        publication: MovePublication<'_>,
    ) {
        let entity = self.get_entity();
        let delta = Vector3::new(pos.x - last_pos.x, pos.y - last_pos.y, pos.z - last_pos.z);
        let cm = move_distance_cm(last_pos, pos);
        if cm > 0 {
            let stat = self.get_movement_statistic();
            self.increment_stat(StatisticCategory::Custom, stat as i32, cm);
        }

        // Vanilla `onGround && !isOnGround && movedUpwards` -> `jumpFromGround`
        if was_on_ground && !requested.on_ground && delta.y > 0.0 {
            self.jump();
        }

        entity
            .on_ground
            .store(requested.on_ground, Ordering::Relaxed);
        entity
            .horizontal_collision
            .store(requested.horizontal_collision, Ordering::Relaxed);
        if requested.on_ground {
            self.try_to_stop_fall_flying();
        }

        let pos_changed = delta.length_squared() > 0.0;
        let rot_changed = match publication {
            MovePublication::Delta => {
                let changed = self.look_changed(requested);
                self.apply_look(requested);
                changed
            }
            MovePublication::Committed(committed) => committed.rotation_changed,
        };

        if pos_changed || rot_changed {
            self.broadcast_client_move(
                last_pos,
                pos,
                requested.on_ground,
                pos_changed,
                rot_changed,
                publication,
            );
        }

        self.do_check_fall_damage(delta, requested.on_ground, requested.horizontal_collision);
        if delta.y > 0.0 {
            self.living_entity.fall_distance.store(0.0);
        }
        if requested.on_ground
            || self.living_entity.has_landed_in_liquid()
            || self.on_climbable()
            || self.is_spectator()
            || entity.is_fall_flying()
            || self.living_entity.is_auto_spin_attack()
        {
            self.living_entity.try_reset_current_impulse_context();
        }

        if pos_changed && let Some(player) = self.world().get_player_by_uuid(self.gameprofile.id) {
            chunker::update_position(&player);
        }
        if delta.length_squared() > 1.0E-5 {
            self.update_last_action_time();
            self.check_location_enchantments(pos, requested.on_ground);
        }
        entity.emit_client_movement(self, delta);
        self.progress_motion(delta);
    }

    pub(crate) fn try_accept_regional_client_move(
        &self,
        server: &Server,
        extent: &RegionChunks,
        movement: ClientMove,
        stationary_action: Option<impl FnOnce() -> Option<()>>,
    ) -> Option<RegionalClientMove> {
        let entity = self.get_entity();
        let from = entity.pos.load();
        let to = movement.position?;
        let difference = to - from;
        let chunk = entity.chunk_pos.load();
        if !self.has_client_loaded()
            || self.is_movement_locked.load(Ordering::Relaxed)
            || self.camera_target_id.load().is_some()
            || self.sleeping_since.load().is_some()
            || self.living_entity.dead.load(Ordering::Relaxed)
            || self.living_entity.health.load() <= 0.0
            || entity.pose.load() != EntityPose::Standing
            || entity.is_fall_flying()
            || self.living_entity.is_auto_spin_attack()
            || entity.touching_water.load(Ordering::Relaxed)
            || entity.touching_lava.load(Ordering::Relaxed)
            || movement.horizontal_collision
            || movement.on_ground != entity.on_ground.load(Ordering::Relaxed)
            || !movement.on_ground && !self.is_flying()
            || stationary_action.is_some()
                && from != to
                && (!movement.on_ground
                    || from.y != to.y
                    || BlockPos::floored_v(to).chunk_position() != chunk)
            || difference.y.abs() > 0.000_02
            || difference.horizontal_length_squared() > 1.0
            || to.x != Self::clamp_horizontal(to.x)
            || to.z != Self::clamp_horizontal(to.z)
            || to.y != Self::clamp_vertical(to.y)
            || server.plugin_manager.has_handlers::<PlayerMoveEvent>()
            || server.plugin_manager.has_handlers::<PlayerInputEvent>()
            || server
                .plugin_manager
                .has_handlers::<PlayerStatisticIncrementEvent>()
        {
            return None;
        }
        let awaiting = self.awaiting_teleport.try_lock().ok()?;
        let vehicle = entity.vehicle.try_lock().ok()?;
        let passengers = entity.passengers.try_lock().ok()?;
        if awaiting.is_some() || vehicle.is_some() || !passengers.is_empty() {
            return None;
        }
        let _checkpoint = server.try_entity_save_guard()?;
        let world = self.world();
        let stamp = RegionalMoveStamp {
            world: world.uuid,
            teleport: self.teleport_id_count.load(Ordering::Relaxed),
            epoch: self.chunk_send_epoch.load(Ordering::Relaxed),
            position: to,
        };
        let pose = self.try_update_regional_pose(
            server,
            extent,
            from,
            to,
            Some(movement),
            stationary_action,
        )?;
        let rotation_changed = pose.rotation_changed;
        let position_changed = from != to;
        if pose.stationary_settled && !rotation_changed {
            return Some(RegionalClientMove::Complete {
                position_changed,
                rotation_changed,
                stamp,
            });
        }
        let (received, all_viewers_queued) = if (position_changed || pose.stationary_settled)
            && (entity.chunk_pos.load() == chunk || pose.grounded_move)
            && stamp.is_current(self, &world)
            && !server.plugin_manager.has_handlers::<PacketSentEvent>()
        {
            self.try_publish_regional_pose(&world, to, movement.on_ground, pose)
                .unwrap_or_default()
        } else {
            (FxHashSet::default(), false)
        };
        if all_viewers_queued
            && (pose.stationary_settled
                || pose.grounded_move
                    && self
                        .try_finish_grounded_move(server, &world, extent, from, to)
                        .is_some())
        {
            return Some(RegionalClientMove::Complete {
                position_changed,
                rotation_changed,
                stamp,
            });
        }
        Some(RegionalClientMove::Committed(CommittedPlayerMove {
            stamp,
            from,
            movement,
            rotation_changed,
            received,
        }))
    }

    /// Completes a level step which needs only the player's local movement accounting.
    fn try_finish_grounded_move(
        &self,
        server: &Server,
        world: &World,
        extent: &RegionChunks,
        from: Vector3<f64>,
        to: Vector3<f64>,
    ) -> Option<()> {
        let entity = self.get_entity();
        let bounds = entity.bounding_box.index.try_write().ok()?;
        let chunk = entity.chunk_pos.load();
        let layout = world.region_layout.load();
        if !std::ptr::eq(bounds.as_ptr(), Arc::as_ptr(&world.entity_index))
            || !self.can_simulate_in(world)
            || entity.is_removed()
            || entity.pos.load() != to
            || !layout
                .owner(chunk)
                .and_then(|owner| layout.chunks(owner))
                .is_some_and(|current| Arc::ptr_eq(current, extent))
            || entity.pose.load() != EntityPose::Standing
            || !self.has_settled_ground_contact()
            || entity.is_sneaking()
            || entity.is_fall_flying()
            || entity.is_swimming()
            || self.is_swimming()
            || entity.is_in_water()
            || entity.is_submerged_in_water()
            || self.living_entity.climbing.load(Ordering::Relaxed)
            || self.is_spectator()
            || server
                .plugin_manager
                .has_handlers::<EntityExhaustionEvent>()
        {
            return None;
        }
        let abilities = self.abilities.try_lock().ok()?;
        if abilities.flying {
            return None;
        }
        let sprinting = entity.is_sprinting();

        let movement = to - from;
        let (walked, flown) = entity.movement_distances(movement, false);
        if walked > entity.next_step.load() {
            return None;
        }
        let active = movement.length_squared() > 1.0E-5;
        let equipment = self.inventory.entity_equipment.try_lock().ok()?;
        if active
            && equipment
                .get(&EquipmentSlot::FEET)
                .get_data_component::<EnchantmentsImpl>()
                .is_some_and(|enchantments| {
                    enchantments
                        .enchantment
                        .iter()
                        .any(|(entry, _)| !entry.effects.location_changed.is_empty())
                })
        {
            return None;
        }
        chunker::try_keep_bedrock_view(self, world, BlockPos::floored_v(from).chunk_position())?;

        let cm = move_distance_cm(from, to);
        if cm > 0 {
            let stat = if sprinting {
                CustomStatistic::SprintOneCm
            } else {
                CustomStatistic::WalkOneCm
            };
            self.stats.try_lock().ok()?.increment_custom(stat, cm);
        }
        entity.movement.store(movement);
        if active {
            self.update_last_action_time();
        }
        entity.move_dist.store(walked);
        entity.fly_dist.store(flown);
        if !abilities.invulnerable
            && let Some(exhaustion) = Self::grounded_movement_exhaustion(movement, sprinting)
        {
            self.hunger_manager.add_exhaustion(exhaustion);
        }
        Some(())
    }

    fn try_publish_regional_pose(
        &self,
        world: &World,
        position: Vector3<f64>,
        on_ground: bool,
        pose: RegionalPose,
    ) -> Option<(FxHashSet<i32>, bool)> {
        let entity = self.get_entity();
        let head = pose.rotation_changed.then(|| self.head_rotation_packet());
        let bedrock = self.bedrock_move_packet(CMovePlayer::MODE_NORMAL, on_ground, 0);
        let tracker = &world.entity_tracker;
        if pose.stationary_settled {
            tracker.try_publish_regional_player_move(
                world,
                entity,
                &self.rotation_packet(on_ground),
                head.as_ref(),
                &bedrock,
            )
        } else {
            tracker.try_publish_regional_player_move(
                world,
                entity,
                &self.absolute_position_packet(position, on_ground),
                head.as_ref(),
                &bedrock,
            )
        }
    }

    pub(crate) fn finish_regional_client_move(&self, committed: &CommittedPlayerMove) -> bool {
        let world = self.world();
        if world.uuid != committed.stamp.world || !self.can_simulate_in(&world) {
            return false;
        }
        if committed.stamp.is_current(self, &world) {
            self.finish_client_move(
                committed.from,
                committed.stamp.position,
                committed.movement,
                committed.movement.on_ground,
                MovePublication::Committed(committed),
            );
            return true;
        }
        if !committed.received.is_empty() {
            self.reconcile_regional_move(&world, committed);
        }
        false
    }

    /// Holds the pose against a new teleport while completing an unobserved edit.
    pub(crate) fn try_edit_at_regional_move(
        &self,
        stamp: RegionalMoveStamp,
        world: &World,
        apply: impl FnOnce() -> Option<()>,
    ) -> Option<()> {
        let awaiting = self.awaiting_teleport.try_lock().ok()?;
        let entity = self.get_entity();
        let bounds = entity.bounding_box.index.try_write().ok()?;
        if awaiting.is_some()
            || entity.is_removed()
            || !std::ptr::eq(bounds.as_ptr(), Arc::as_ptr(&world.entity_index))
            || !stamp.is_current(self, world)
        {
            return None;
        }
        apply()
    }

    fn reconcile_regional_move(&self, world: &World, committed: &CommittedPlayerMove) {
        let entity = self.get_entity();
        let pos = entity.pos.load();
        if entity.is_removed()
            || pos == committed.stamp.position && !self.look_changed(committed.movement)
        {
            return;
        }
        // A teleport can supersede an already sent pose. Across chunks, only
        // correct an existing pairing once its new terrain has been queued.
        let chunk = entity.chunk_pos.load();
        let same_chunk = chunk == BlockPos::floored_v(committed.stamp.position).chunk_position();
        let on_ground = entity.on_ground.load(Ordering::Relaxed);
        let already_received = |viewer: &Self| {
            committed.received.contains(&viewer.entity_id())
                && (same_chunk
                    || viewer.can_simulate_in(world)
                        && viewer
                            .watched_section
                            .load()
                            .is_within_distance(chunk.x, chunk.y)
                        && viewer
                            .chunk_sender
                            .try_lock()
                            .is_ok_and(|sender| sender.is_chunk_ready(&chunk)))
        };
        world.send_to_tracking_players_filtered_editioned(
            entity,
            &self.absolute_position_packet(pos, on_ground),
            &self.bedrock_move_packet(CMovePlayer::MODE_NORMAL, on_ground, 0),
            already_received,
        );
        world.send_to_tracking_players_filtered(
            entity,
            &self.head_rotation_packet(),
            already_received,
        );
    }

    /// With a current-protocol regional input, commit position and look under the player's
    /// bounds guard. A world-owned move already handles look in its normal effects.
    fn try_update_regional_pose(
        &self,
        server: &Server,
        extent: &RegionChunks,
        from: Vector3<f64>,
        to: Vector3<f64>,
        movement: Option<ClientMove>,
        stationary_action: Option<impl FnOnce() -> Option<()>>,
    ) -> Option<RegionalPose> {
        if ![to.x, to.y, to.z].into_iter().all(f64::is_finite) || from == to && movement.is_none() {
            return None;
        }
        let _native = server.plugin_manager.try_without_native_code()?;
        let entity = self.get_entity();
        let world = self.world();
        let origin = entity.chunk_pos.load();
        let owner = world.region_layout.load().owner(origin)?;
        let owns = |chunk| {
            let layout = world.region_layout.load();
            layout.owner(chunk) == Some(owner)
                && layout
                    .chunks(owner)
                    .is_some_and(|live| Arc::ptr_eq(live, extent))
        };
        let bounds_owner = entity.bounding_box.index.try_write().ok()?;
        if !std::ptr::eq(bounds_owner.as_ptr(), Arc::as_ptr(&world.entity_index))
            || (movement.is_some() && entity.pose.load() != EntityPose::Standing)
        {
            return None;
        }
        let owns_player = || {
            self.can_simulate_in(&world)
                && !entity.is_removed()
                && entity.pos.load() == from
                && entity.chunk_pos.load() == origin
                && owns(origin)
        };
        let commit_look = |movement| {
            let rotation_changed = self.look_changed(movement);
            self.apply_look(movement);
            rotation_changed
        };
        if from == to {
            return self.try_commit_stationary_pose(
                &world,
                movement.filter(|_| owns_player())?,
                stationary_action,
            );
        }
        let bounds =
            Entity::make_bounding_box(entity.entity_type, to, &entity.entity_dimension.load());
        let swept = entity.bounding_box.load().minmax(&bounds);
        let destination = BlockPos::floored_v(to);
        let mut first = BlockPos::floored_v(swept.min);
        let last = swept.max_block_pos();
        let floor = movement
            .filter(|input| input.on_ground)
            .map(|_| from.y.round() as i32 - 1);
        if let Some(floor) = floor {
            if (from.y - f64::from(floor + 1)).abs() > 0.000_02 {
                return None;
            }
            first.0.y = floor;
        }
        let can_finish_grounded_move = floor.is_some() && from.y == to.y;
        // Reuse the normal position fields without reacquiring the pinned chunk or index.
        world
            .level
            .try_with_regional_path(
                first,
                last,
                owns,
                |read| {
                    (movement.is_none() || clear_movement_path(first, last, floor, read)).then(|| {
                        let grounded_move = can_finish_grounded_move && read(destination).is_some_and(|id| {
                            !id.to_block().has_tag(&pumpkin_data::tag::Block::MINECRAFT_CLIMBABLE)
                        });
                        (destination, grounded_move)
                    })
                },
                |grounded_move, biome| {
                    world.entity_index.try_move_regional_player(
                        &world,
                        self,
                        bounds,
                        movement.map(|_| swept.expand(1.0, 0.5, 1.0)),
                        || {
                            if !owns_player() {
                                return None;
                            }
                            entity.set_pos_with(
                                to,
                                || entity.bounding_box.value.store(bounds),
                                |_| biome,
                            );
                            Some(RegionalPose {
                                rotation_changed: movement.is_some_and(commit_look),
                                stationary_settled: false,
                                grounded_move,
                            })
                        },
                    )
                },
            )
            .flatten()
            .inspect(|_| {
                tracing::trace!(player = %self.gameprofile.name, region = owner, position = ?to, "Committed player position in its region");
            })
    }

    fn try_commit_stationary_pose(
        &self,
        world: &World,
        movement: ClientMove,
        action: Option<impl FnOnce() -> Option<()>>,
    ) -> Option<RegionalPose> {
        let rotation_changed = self.look_changed(movement);
        let entity = self.get_entity();
        let stationary_settled = movement.on_ground
            && self.has_settled_ground_contact()
            && entity.movement.load() == Vector3::default()
            && entity.move_dist.load() <= entity.next_step.load()
            && matches!(
                world.level.loaded_chunks.try_get(&entity.chunk_pos.load()),
                TryResult::Present(_)
            );
        if let Some(commit) = action {
            // An inventory action can finish here only when no pose packet or
            // other movement effect needs a second owner to finish the input.
            if rotation_changed || !stationary_settled {
                return None;
            }
            commit()?;
        } else if !rotation_changed && !stationary_settled {
            return None;
        }
        self.apply_look(movement);
        Some(RegionalPose {
            rotation_changed,
            stationary_settled,
            grounded_move: false,
        })
    }

    fn has_settled_ground_contact(&self) -> bool {
        let entity = self.get_entity();
        entity.on_ground.load(Ordering::Relaxed)
            && !entity.horizontal_collision.load(Ordering::Relaxed)
            && self.living_entity.fall_distance.load() == 0.0
            && !self.living_entity.is_in_post_impulse_grace_time()
            && !self
                .living_entity
                .is_ignoring_fall_damage_from_current_impulse()
    }

    fn look_changed(&self, requested: ClientMove) -> bool {
        let entity = self.get_entity();
        requested
            .yaw
            .is_some_and(|yaw| (wrap_degrees(yaw) % 360.0 - entity.yaw.load()).abs() > f32::EPSILON)
            || requested.pitch.is_some_and(|pitch| {
                (wrap_degrees(pitch).clamp(-90.0, 90.0) - entity.pitch.load()).abs() > f32::EPSILON
            })
            || requested.head_yaw.is_some_and(|head_yaw| {
                (wrap_degrees(head_yaw) % 360.0 - entity.head_yaw.load()).abs() > f32::EPSILON
            })
    }

    /// Look without position: teleport pending, movement locked or riding.
    fn apply_and_broadcast_look(&self, requested: ClientMove) {
        let rot_changed = self.look_changed(requested);
        self.apply_look(requested);
        if rot_changed {
            let entity = self.get_entity();
            let pos = entity.pos.load();
            let on_ground = entity.on_ground.load(Ordering::Relaxed);
            self.broadcast_client_move(pos, pos, on_ground, false, true, MovePublication::Delta);
        }
    }

    fn apply_look(&self, requested: ClientMove) {
        let entity = self.get_entity();
        let head_yaw = requested.head_yaw.map(|yaw| wrap_degrees(yaw) % 360.0);
        if requested.yaw.is_some() || requested.pitch.is_some() {
            let yaw = requested
                .yaw
                .map_or_else(|| entity.yaw.load(), |yaw| wrap_degrees(yaw) % 360.0);
            let pitch = requested
                .pitch
                .map_or_else(|| entity.pitch.load(), wrap_degrees);
            entity.set_rotation_with_head(yaw, pitch, head_yaw.unwrap_or(yaw));
        } else if let Some(head_yaw) = head_yaw {
            entity.head_yaw.store(head_yaw);
        }
    }

    fn kick_invalid_movement(&self) {
        self.client.try_kick(
            DisconnectReason::Kicked,
            &TextComponent::translate_cross(
                translation::java::MULTIPLAYER_DISCONNECT_INVALID_PLAYER_MOVEMENT,
                translation::java::MULTIPLAYER_DISCONNECT_INVALID_PLAYER_MOVEMENT,
                [],
            ),
        );
    }

    fn absolute_position_packet(&self, pos: Vector3<f64>, on_ground: bool) -> CEntityPositionSync {
        let entity = self.get_entity();
        CEntityPositionSync::new(
            entity.entity_id.into(),
            pos,
            Vector3::default(),
            entity.yaw.load(),
            entity.pitch.load(),
            on_ground,
        )
    }

    fn head_rotation_packet(&self) -> CHeadRot {
        let entity = self.get_entity();
        CHeadRot::new(
            entity.entity_id.into(),
            Self::packed_rot(entity.head_yaw.load()),
        )
    }

    fn rotation_packet(&self, on_ground: bool) -> CUpdateEntityRot {
        let entity = self.get_entity();
        CUpdateEntityRot::new(
            entity.entity_id.into(),
            Self::packed_rot(entity.yaw.load()),
            Self::packed_rot(entity.pitch.load()),
            on_ground,
        )
    }

    fn broadcast_client_move(
        &self,
        last_pos: Vector3<f64>,
        pos: Vector3<f64>,
        on_ground: bool,
        pos_changed: bool,
        rot_changed: bool,
        publication: MovePublication<'_>,
    ) {
        let entity = self.get_entity();
        let world = entity.world.load_full();
        let entity_id = entity.entity_id;
        let delta = pos - last_pos;
        let received = match publication {
            MovePublication::Committed(committed) if !committed.received.is_empty() => {
                Some(&committed.received)
            }
            _ => None,
        };
        let outstanding =
            |viewer: &Self| received.is_none_or(|ids| !ids.contains(&viewer.entity_id()));

        let long_move = delta.length_squared() >= 64.0;
        if pos_changed && (long_move || matches!(publication, MovePublication::Committed(_))) {
            let mode = if long_move {
                CMovePlayer::MODE_TELEPORT
            } else {
                CMovePlayer::MODE_NORMAL
            };
            let java_move = self.absolute_position_packet(pos, on_ground);
            let bedrock_move = self.bedrock_move_packet(mode, on_ground, 0);
            if received.is_some() {
                world.send_to_tracking_players_filtered_editioned(
                    entity,
                    &java_move,
                    &bedrock_move,
                    outstanding,
                );
            } else {
                world.send_to_tracking_players_editioned(entity, &java_move, &bedrock_move);
            }
            // Bedrock ignores head yaw on teleport -> a normal move re-asserts it
            if long_move {
                world.send_to_tracking_players_bedrock(
                    entity,
                    &self.bedrock_move_packet(CMovePlayer::MODE_NORMAL, on_ground, 0),
                );
            }
        } else if pos_changed && rot_changed {
            world.send_to_tracking_players_editioned(
                entity,
                &CUpdateEntityPosRot::new(
                    entity_id.into(),
                    packed_delta(last_pos, pos),
                    Self::packed_rot(entity.yaw.load()),
                    Self::packed_rot(entity.pitch.load()),
                    on_ground,
                ),
                &self.bedrock_move_packet(CMovePlayer::MODE_NORMAL, on_ground, 0),
            );
        } else if pos_changed {
            world.send_to_tracking_players_editioned(
                entity,
                &CUpdateEntityPos::new(entity_id.into(), packed_delta(last_pos, pos), on_ground),
                &self.bedrock_move_packet(CMovePlayer::MODE_NORMAL, on_ground, 0),
            );
        } else if rot_changed {
            let java = self.rotation_packet(on_ground);
            let bedrock = self.bedrock_move_packet(CMovePlayer::MODE_NORMAL, on_ground, 0);
            if received.is_some() {
                world.send_to_tracking_players_filtered_editioned(
                    entity,
                    &java,
                    &bedrock,
                    outstanding,
                );
            } else {
                world.send_to_tracking_players_editioned(entity, &java, &bedrock);
            }
        }

        if rot_changed {
            let head = self.head_rotation_packet();
            if received.is_some() {
                world.send_to_tracking_players_filtered(entity, &head, outstanding);
            } else {
                world.send_to_tracking_players(entity, &head);
            }
        }
    }
}

fn move_distance_cm(from: Vector3<f64>, to: Vector3<f64>) -> i32 {
    (from.squared_distance_to_vec(&to).sqrt() * 100.0) as i32
}

fn clear_movement_path(
    first: BlockPos,
    last: BlockPos,
    floor: Option<i32>,
    read: &dyn Fn(BlockPos) -> Option<BlockStateId>,
) -> bool {
    for x in first.0.x..=last.0.x {
        for z in first.0.z..=last.0.z {
            for y in first.0.y..=last.0.y {
                let Some(id) = read(BlockPos::new(x, y, z)) else {
                    return false;
                };
                let state = id.to_state();
                if if floor == Some(y) {
                    !state.is_full_cube()
                        || World::get_fluid_from_state_id(id).id
                            != pumpkin_data::fluid::Fluid::EMPTY.id
                } else {
                    !state.is_air()
                } {
                    return false;
                }
            }
        }
    }
    true
}

fn packed_delta(last_pos: Vector3<f64>, pos: Vector3<f64>) -> Vector3<i16> {
    Vector3::new(
        pos.x.mul_add(4096.0, -(last_pos.x * 4096.0)) as i16,
        pos.y.mul_add(4096.0, -(last_pos.y * 4096.0)) as i16,
        pos.z.mul_add(4096.0, -(last_pos.z * 4096.0)) as i16,
    )
}

fn dismount_if_riding(entity: &crate::entity::Entity) {
    if let Some(vehicle) = entity.get_vehicle() {
        vehicle.get_entity().remove_passenger(entity.entity_id);
    }
}
