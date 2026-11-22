#[allow(clippy::wildcard_imports)]
use super::*;
use pumpkin_data::entity::EntityType;
use pumpkin_protocol::bedrock::client::set_actor_data::{
    CSetActorData, MetadataValue, PropertySyncData, SyncedActorDataList, entity_data_key,
};
use pumpkin_protocol::bedrock::client::{CMoveActorAbsolute, CMovePlayer};
use pumpkin_protocol::bedrock::server::player_auth_input::{InputMode, InteractionModel};
use pumpkin_protocol::serial::PacketRead;
use pumpkin_util::math::pack_degrees;
use pumpkin_util::math::vector2::Vector2;
use pumpkin_util::math::vector3::Vector3;

use crate::entity::EntityBase;
use crate::entity::player::{
    ClientMove, ClientMoveOutcome, PlayerInput, client_move::CommittedPlayerMove,
};
use crate::entity::vehicle::boat::BEDROCK_YAW_OFFSET;
use crate::world::RegionChunks;

const TELEPORT_RESEND_INPUTS: u32 = 20;

impl BedrockClient {
    pub(in crate::net::bedrock) fn try_commit_regional_auth_move(
        &self,
        player: &Player,
        server: &Server,
        extent: &RegionChunks,
        packet: &SPlayerAuthInput,
    ) -> Option<CommittedPlayerMove> {
        let flags = &packet.input_data;
        let needs_world = [
            InputData::HandledTeleport,
            InputData::ClientPredictedVehicle,
            InputData::StartSprinting,
            InputData::StopSprinting,
            InputData::StartSneaking,
            InputData::StopSneaking,
            InputData::StartFlying,
            InputData::StopFlying,
            InputData::StartGliding,
            InputData::StopGliding,
            InputData::StartSwimming,
            InputData::StopSwimming,
            InputData::StartCrawling,
            InputData::StopCrawling,
            InputData::StartSpinAttack,
            InputData::StopSpinAttack,
            InputData::HorizontalCollision,
        ];
        if packet.item_interaction.is_some()
            || packet.item_stack_request.is_some()
            || packet.block_actions.is_some()
            || needs_world.into_iter().any(|flag| flags.get(flag as usize))
            || ![
                packet.yaw,
                packet.pitch,
                packet.head_yaw,
                packet.delta.x,
                packet.delta.y,
                packet.delta.z,
            ]
            .into_iter()
            .all(f32::is_finite)
            || player.get_entity().is_sneaking()
            || player_input_from_auth(packet, false).bits() != player.player_input().bits()
        {
            return None;
        }
        let movement = ClientMove {
            position: Some(player.feet_from_bedrock_pos(packet.position.to_f64())),
            yaw: Some(packet.yaw),
            pitch: Some(packet.pitch),
            head_yaw: Some(packet.head_yaw),
            on_ground: flags.get(InputData::VerticalCollision as usize)
                && self.client_delta.load().y < 0.0,
            horizontal_collision: false,
        };
        let committed = player.try_commit_regional_client_move(server, extent, movement)?;
        self.store_input_state(packet);
        Some(committed)
    }

    pub fn handle_player_auth_input(
        &self,
        player: &Arc<Player>,
        packet: SPlayerAuthInput,
        server: &Arc<Server>,
    ) {
        let mut spin_attack_prediction = None;
        self.apply_player_auth_input(player, packet, server, &mut spin_attack_prediction, None);
        self.reconcile_spin_attack_prediction(player, spin_attack_prediction);
    }

    pub(in crate::net::bedrock) fn apply_player_auth_input(
        &self,
        player: &Arc<Player>,
        packet: SPlayerAuthInput,
        server: &Arc<Server>,
        spin_attack_prediction: &mut Option<bool>,
        extent: Option<&RegionChunks>,
    ) {
        if player.living_entity.dead.load(Ordering::Relaxed)
            || player.living_entity.health.load() <= 0.0
        {
            self.reset_horse_jump();
            return;
        }
        if self.await_teleport(player, &packet) {
            // Stale poses before a respawn teleport must not count as client motion.
            player.get_entity().movement.store(Vector3::default());
            self.reset_horse_jump();
            return;
        }
        if !player.has_client_loaded() {
            return;
        }

        let entity = player.get_entity();
        let flags = &packet.input_data;
        let was_riding = entity.has_vehicle();
        // Like Geyser: the motion this tick started with was downward. Landing can already
        // have cleared the end delta (gliding into the ground).
        let on_ground = flags.get(InputData::VerticalCollision as usize)
            && self.client_delta.load().y < 0.0
            && !was_riding;
        let horizontal_collision = flags.get(InputData::HorizontalCollision as usize);

        self.store_input_state(&packet);

        let sneaking = sneaking_from_auth(player, flags);
        let was_sneaking = entity.is_sneaking();
        player.apply_player_input(server, player_input_from_auth(&packet, sneaking));
        if entity.is_sneaking() != was_sneaking {
            player.update_bedrock_shield_sneaking();
        }

        let started_sprint = flags.get(InputData::StartSprinting as usize);
        let stopped_sprint = flags.get(InputData::StopSprinting as usize);
        if started_sprint && !stopped_sprint {
            player.apply_sprint_input(server, true);
        } else if stopped_sprint && !started_sprint {
            player.apply_sprint_input(server, false);
        }

        if flags.get(InputData::StopSpinAttack as usize) {
            *spin_attack_prediction = Some(false);
        } else if flags.get(InputData::StartSpinAttack as usize) {
            *spin_attack_prediction = Some(true);
        }

        if flags.get(InputData::StartFlying as usize) {
            player.apply_flight_input(server, true);
        } else if flags.get(InputData::StopFlying as usize) {
            player.apply_flight_input(server, false);
        }

        // A stop in the same input takes precedence over starting a glide.
        if flags.get(InputData::StopGliding as usize) {
            player.apply_bedrock_glide_input(server, false);
        } else if flags.get(InputData::StartGliding as usize) {
            player.apply_bedrock_glide_input(server, true);
        }

        // A ridden input can still carry the vehicle's coordinates on the dismount tick.
        let position = (!was_riding && !flags.get(InputData::ClientPredictedVehicle as usize))
            .then(|| player.feet_from_bedrock_pos(packet.position.to_f64()));
        let movement = ClientMove {
            position,
            yaw: Some(packet.yaw),
            pitch: Some(packet.pitch),
            head_yaw: Some(packet.head_yaw),
            on_ground,
            horizontal_collision,
        };
        let outcome = player.apply_client_move_with_extent(server, movement, extent);

        // Use the committed position so a swim can start in the same input that enters water.
        if flags.get(InputData::StopSwimming as usize) {
            player.apply_bedrock_swim_input(false);
        } else if flags.get(InputData::StartSwimming as usize) {
            player.apply_bedrock_swim_input(true);
        }

        if flags.get(InputData::StopCrawling as usize) {
            player.apply_bedrock_crawl_input(false);
        } else if flags.get(InputData::StartCrawling as usize) {
            player.apply_bedrock_crawl_input(true);
        }

        if outcome == ClientMoveOutcome::Applied {
            self.apply_vehicle_input(server, player, &packet);
        } else if was_riding {
            self.reset_horse_jump();
        }

        if let Some(interaction) = packet.item_interaction {
            self.handle_inventory_action(player, interaction.into());
        }

        // Inventory moves can ride inside the input packet, like the standalone request
        if let Some(request) = packet.item_stack_request {
            self.handle_item_stack_request(
                player,
                pumpkin_protocol::bedrock::server::item_stack_request::SItemStackRequest {
                    requests: vec![request],
                },
            );
        }

        if let Some(block_actions) = packet.block_actions {
            for action in &block_actions {
                self.apply_player_action(
                    player,
                    server,
                    action.action,
                    action.block_pos,
                    spin_attack_prediction,
                );
            }
        }
    }
}

impl BedrockClient {
    fn apply_vehicle_input(
        &self,
        server: &Arc<Server>,
        player: &Arc<Player>,
        packet: &SPlayerAuthInput,
    ) {
        const MAX_STEP_SQUARED: f64 = 100.0;
        const GROUND_PROBE_DEPTH: f64 = 0.001;

        let flags = &packet.input_data;
        if !flags.get(InputData::ClientPredictedVehicle as usize) {
            return;
        }
        let Some(vehicle) = player.controlled_vehicle() else {
            self.reset_horse_jump();
            return;
        };
        let base = vehicle.get_entity();
        let is_boat = base.entity_type.boat.is_some();
        if (!is_boat && base.entity_type != &EntityType::HORSE)
            || packet.vehicle_unique_id.map(|id| id.0) != Some(i64::from(base.entity_id))
        {
            return;
        }
        let Some(rotation) = packet.vehicle_rotation else {
            return;
        };

        let before = base.pos.load();
        let target = vehicle.feet_from_bedrock_pos(packet.position.to_f64());
        let mut motion = target - before;
        let distance_squared = motion.length_squared();
        let world = player.world();
        let bounds = base.bounding_box.load();
        if !distance_squared.is_finite()
            || distance_squared > MAX_STEP_SQUARED
            || !rotation.x.is_finite()
            || !rotation.y.is_finite()
            || !world
                .worldborder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .collision_shape_contains(&bounds.shift(motion))
        {
            self.correct_vehicle_prediction(vehicle.as_ref());
            return;
        }

        let supported = |offset: Vector3<f64>| {
            !world.is_space_empty(bounds.shift(offset.add_raw(0.0, -GROUND_PROBE_DEPTH, 0.0)))
        };
        let mut collision = base.collide(motion, vehicle.as_ref());
        let mut corrected_jump = false;
        if !is_boat {
            let grounded = base.on_ground.load(Ordering::Relaxed) || supported(Vector3::default());
            let Some(suppress_lift) =
                self.apply_horse_jump_input(server, player, vehicle.as_ref(), packet, grounded)
            else {
                return;
            };
            if base.pos.load() != before {
                return;
            }
            if suppress_lift && motion.y > GROUND_PROBE_DEPTH {
                let requested_lift = motion.y;
                motion.y = 0.0;
                collision = base.collide(motion, vehicle.as_ref());
                // Derive any height gain from the shared terrain step, independently of the jump.
                if let Some(step) =
                    base.try_step_up(motion, collision.movement, vehicle.as_ref(), grounded)
                    && supported(step)
                {
                    collision.movement = step;
                    collision.horizontal = (step - motion).horizontal_length_squared()
                        > GROUND_PROBE_DEPTH * GROUND_PROBE_DEPTH;
                }
                corrected_jump = (collision.movement.y - requested_lift).abs() > GROUND_PROBE_DEPTH;
            }
        }
        let on_ground =
            collision.supporting.is_some() || (motion.y <= 0.0 && supported(collision.movement));
        base.horizontal_collision
            .store(collision.horizontal, Ordering::Relaxed);
        base.vertical_collision
            .store(collision.vertical, Ordering::Relaxed);
        let yaw = if is_boat {
            rotation.y - BEDROCK_YAW_OFFSET
        } else {
            rotation.y
        };
        player.apply_client_vehicle_move(
            vehicle.as_ref(),
            before + collision.movement,
            yaw,
            rotation.x,
            on_ground,
        );

        if is_boat {
            // Bedrock labels the paddles from the opposite side of the hull.
            let forward = flags.get(InputData::Up as usize);
            vehicle.set_paddle_state(
                forward || flags.get(InputData::PaddlingRight as usize),
                forward || flags.get(InputData::PaddlingLeft as usize),
            );
        }
        if corrected_jump || collision.horizontal || collision.vertical {
            self.correct_vehicle_prediction(vehicle.as_ref());
        }
    }

    fn correct_vehicle_prediction(&self, vehicle: &dyn EntityBase) {
        let base = vehicle.get_entity();
        let [pitch, yaw, head_yaw, _] = base.bedrock_rotation();
        let mut header = CMoveActorAbsolute::FLAG_TELEPORT | CMoveActorAbsolute::FLAG_FORCE_MOVE;
        if base.on_ground.load(Ordering::Relaxed) {
            header |= CMoveActorAbsolute::FLAG_ON_GROUND;
        }
        self.try_enqueue_client_packet(&CMoveActorAbsolute {
            actor_runtime_id: VarULong(base.entity_id as u64),
            header,
            position: vehicle.bedrock_pos().to_f32_lossy(),
            rotation_x: pack_degrees(pitch),
            rotation_y: pack_degrees(yaw),
            rotation_y_head: pack_degrees(head_yaw),
        });
    }

    pub(crate) fn reconcile_spin_attack_prediction(
        &self,
        player: &Player,
        spin_attack_prediction: Option<bool>,
    ) {
        let Some(predicted) = spin_attack_prediction else {
            return;
        };
        if player.living_entity.is_auto_spin_attack() == predicted {
            return;
        }

        let entity = player.get_entity();
        let mut actor_data = SyncedActorDataList::new();
        actor_data.set(
            entity_data_key::FLAGS,
            MetadataValue::Int64(entity.bedrock_flags.load(Ordering::Relaxed)),
        );
        self.try_enqueue_client_packet(&CSetActorData {
            target_runtime_id: VarULong(entity.entity_id as u64),
            actor_data,
            synced_properties: PropertySyncData::default(),
            tick: VarULong(0),
        });
    }

    /// Client state at this input, which push motions build on.
    fn store_input_state(&self, packet: &SPlayerAuthInput) {
        self.input_tick.store(packet.tick.0, Ordering::Relaxed);
        // Client predicted end-of-tick velocity. Knockback is an absolute `SetActorMotion`.
        // later entity pushes layer on this delta once that motion is no longer in flight.
        self.client_delta.store(packet.delta.to_f64());
    }

    pub(crate) fn send_teleport(&self, player: &Player) {
        if self.is_closed() {
            return;
        }
        let (id, packet) = {
            let pending = player
                .awaiting_teleport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let packet = player.bedrock_move_packet(
                CMovePlayer::MODE_TELEPORT,
                false,
                self.input_tick.load(Ordering::Relaxed),
            );
            self.activity.sent_teleport_id.store(None);
            self.teleport_unconfirmed_inputs.store(0, Ordering::Relaxed);
            self.teleport_sent_pos.store(packet.position);
            (pending.map(|(id, _)| id.0), packet)
        };
        let data = match self.serialize_packet(&packet) {
            Ok(data) => data,
            Err(err) => {
                tracing::error!("Failed to serialize Bedrock teleport: {err}");
                return;
            }
        };
        let (outgoing, extras) = self.translate_outgoing(data.clone(), true);
        let Some(outgoing) = outgoing else {
            self.queue_extra_packets(extras);
            return;
        };
        let unchanged = outgoing == data;
        if self.queue_translated_packet(outgoing, extras) && unchanged && id.is_some() {
            // Packet hooks run without the pending-state lock and can schedule a new teleport.
            let pending = player
                .awaiting_teleport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.map(|(id, _)| id.0) == id && self.teleport_sent_pos.load() == packet.position
            {
                self.activity.sent_teleport_id.store(id);
            }
        }
    }

    fn reached_teleport_position(&self, position: Vector3<f32>) -> bool {
        const TELEPORT_ERROR: f32 = 0.1;
        // Compare with the f32 position actually sent.
        let target = self.teleport_sent_pos.load();
        (position.x - target.x).abs() < TELEPORT_ERROR
            && (position.y - target.y).abs() < TELEPORT_ERROR
            && (position.z - target.z).abs() < TELEPORT_ERROR
    }

    pub(in crate::net::bedrock) fn try_regional_teleport_input(
        &self,
        player: &Player,
        mut payload: &[u8],
        owns_player: impl FnOnce() -> bool,
    ) -> bool {
        let Some(sent_id) = self.activity.sent_teleport_id.load() else {
            return false;
        };
        let Ok(mut pending) = player.awaiting_teleport.try_lock() else {
            return false;
        };
        if !pending.is_some_and(|(id, _)| id.0 == sent_id)
            || self.activity.sent_teleport_id.load() != Some(sent_id)
            || player.living_entity.dead.load(Ordering::Relaxed)
            || player.living_entity.health.load() <= 0.0
        {
            return false;
        }
        let Ok(input) = SPlayerAuthInput::read(&mut payload) else {
            return false;
        };
        if !payload.is_empty() || !owns_player() {
            return false;
        }
        let reached = self.reached_teleport_position(input.position);
        if reached {
            *pending = None;
            self.activity.sent_teleport_id.store(None);
            player.set_client_loaded(true);
        } else {
            // A resend queues a new authoritative packet on the world path.
            let missed = self.teleport_unconfirmed_inputs.load(Ordering::Relaxed);
            if missed >= TELEPORT_RESEND_INPUTS - 1 {
                return false;
            }
            self.teleport_unconfirmed_inputs
                .store(missed + 1, Ordering::Relaxed);
        }
        drop(pending);
        player.get_entity().movement.store(Vector3::default());
        self.reset_horse_jump();
        if reached {
            tracing::trace!(
                player = %player.gameprofile.name,
                teleport_id = sent_id,
                input_tick = input.tick.0,
                "Confirmed Bedrock teleport on its region"
            );
        }
        true
    }

    /// Inputs are ignored until the client reaches a pending teleport, so stale
    /// positions (e.g. the death spot before a respawn) never count as movement or a fall.
    /// Returns true while still waiting.
    fn await_teleport(&self, player: &Arc<Player>, packet: &SPlayerAuthInput) -> bool {
        let mut awaiting = player
            .awaiting_teleport
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if awaiting.is_none() {
            return false;
        }
        if self.reached_teleport_position(packet.position) {
            *awaiting = None;
            self.activity.sent_teleport_id.store(None);
            drop(awaiting);
            // Bedrock sends no `PlayerLoaded` after a respawn, reaching the spawn stands in
            player.set_client_loaded(true);
        } else if self
            .teleport_unconfirmed_inputs
            .fetch_add(1, Ordering::Relaxed)
            + 1
            >= TELEPORT_RESEND_INPUTS
        {
            drop(awaiting);
            player.send_bedrock_teleport(self);
        }
        true
    }
}

fn sneaking_from_auth(
    player: &Player,
    flags: &pumpkin_protocol::codec::bitset::Bitset<66>,
) -> bool {
    if player.is_flying() {
        return flags.get(InputData::Descend as usize) || flags.get(InputData::SneakDown as usize);
    }
    // Stop wins when both edges arrive on the same tick.
    if flags.get(InputData::StopSneaking as usize) {
        false
    } else if flags.get(InputData::StartSneaking as usize) {
        true
    } else {
        player.get_entity().is_sneaking()
    }
}

fn player_input_from_auth(packet: &SPlayerAuthInput, sneaking: bool) -> PlayerInput {
    let flags = &packet.input_data;
    let mouse = packet.input_mode.0 == InputMode::Mouse as u32;
    let classic_touch = packet.input_mode.0 == InputMode::Touch as u32
        && packet.interaction_model.0 == InteractionModel::Classic as i32;
    let (up, down, left, right) = if mouse || classic_touch {
        digital_move(flags, classic_touch)
    } else {
        analog_move(packet.analog_move)
    };

    let mut input = PlayerInput::empty();
    input.set(PlayerInput::FORWARD, up);
    input.set(PlayerInput::BACKWARD, down);
    input.set(PlayerInput::LEFT, left);
    input.set(PlayerInput::RIGHT, right);
    input.set(
        PlayerInput::JUMP,
        flags.get(InputData::JumpCurrentRaw as usize)
            || flags.get(InputData::JumpDown as usize)
            || flags.get(InputData::AutoJumpingInWater as usize),
    );
    input.set(PlayerInput::SNEAK, sneaking);
    input.set(
        PlayerInput::SPRINT,
        flags.get(InputData::SprintDown as usize),
    );
    input
}

fn digital_move(
    flags: &pumpkin_protocol::codec::bitset::Bitset<66>,
    classic_touch: bool,
) -> (bool, bool, bool, bool) {
    let mut up = flags.get(InputData::Up as usize);
    let mut down = flags.get(InputData::Down as usize);
    let mut left = flags.get(InputData::Left as usize);
    let mut right = flags.get(InputData::Right as usize);
    if classic_touch {
        if flags.get(InputData::UpLeft as usize) {
            up = true;
            left = true;
        }
        if flags.get(InputData::UpRight as usize) {
            up = true;
            right = true;
        }
        if flags.get(InputData::DownLeft as usize) {
            down = true;
            left = true;
        }
        if flags.get(InputData::DownRight as usize) {
            down = true;
            right = true;
        }
    }
    (up, down, left, right)
}

fn analog_move(analog: Vector2<f32>) -> (bool, bool, bool, bool) {
    (
        analog.y > 0.0,
        analog.y < 0.0,
        analog.x > 0.0,
        analog.x < 0.0,
    )
}
