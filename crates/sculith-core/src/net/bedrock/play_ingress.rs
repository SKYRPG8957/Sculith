use std::sync::{Arc, Weak, atomic::Ordering::Relaxed};

use sculith_protocol::{
    RawPacket,
    bedrock::{
        network_stack_latency::NetworkStackLatency,
        server::{
            client_cache_blob_status::SClientCacheBlobStatus,
            inventory_transaction::{HandSlot, SInventoryTransaction, TransactionData},
            player_auth_input::SPlayerAuthInput,
        },
    },
    codec::{var_int::VarInt, var_ulong::VarULong},
    packet::Packet,
    serial::PacketRead,
};
use sculith_util::math::{position::BlockPos, vector2::Vector2};

use crate::{
    entity::{
        EntityBase,
        player::{
            Player,
            client_move::{CommittedPlayerMove, RegionalClientMove},
        },
    },
    plugin::server::packet::{PacketReceivedEvent, PacketSentEvent},
    server::{Server, player_commands::PlayerCommandIngress},
    world::{RegionChunks, World},
};

use super::BedrockClient;

const REGIONAL_PACKET_BUDGET: usize = 8;
const REGIONAL_MOVEMENT_BYTES: usize = 512;
const REGIONAL_ATTACK_BYTES: usize = 512;

// The raw packet remains available for translation, hook edits and changes of owner.
pub(super) struct QueuedPlayInput {
    packet: RawPacket,
    movement: Option<PreparedMovement>,
    attack: Option<PreparedAttack>,
}

impl From<RawPacket> for QueuedPlayInput {
    fn from(packet: RawPacket) -> Self {
        Self {
            packet,
            movement: None,
            attack: None,
        }
    }
}

struct PreparedAttack {
    extent: Weak<[Vector2<i32>]>,
    packet: SInventoryTransaction,
}

struct PreparedMovement {
    extent: Weak<[Vector2<i32>]>,
    input: SPlayerAuthInput,
    fully_read: bool,
    committed: Option<CommittedPlayerMove>,
}

enum RegionalMovePreparation {
    Consumed,
    Deferred,
    Unprepared,
}

fn owned_extent(owner: &Weak<[Vector2<i32>]>, player: &Player) -> Option<RegionChunks> {
    let extent = owner.upgrade()?;
    let world = player.world();
    let layout = world.region_layout.load();
    (player.can_simulate_in(&world)
        && layout
            .owner(player.get_entity().chunk_pos.load())
            .and_then(|owner| layout.chunks(owner))
            .is_some_and(|current| Arc::ptr_eq(current, &extent)))
    .then_some(extent)
}

impl BedrockClient {
    pub(crate) fn regional_work(&self) -> &std::sync::Mutex<()> {
        &self.activity.regional_work
    }

    pub(crate) fn inventory_edits(&self) -> &crate::server::PlayerInventoryEdits {
        &self.activity.inventory_edits
    }

    pub(crate) fn with_idle_play_ingress<R>(
        &self,
        player: &Player,
        apply: impl FnOnce() -> R,
    ) -> Option<R> {
        let head = self.activity.inbound_head.try_lock().ok()?;
        (head.is_none() && player.inbound_packets.is_empty()).then(apply)
    }

    pub(crate) fn with_idle_player_inventory(&self, player: &Player, apply: impl FnOnce()) {
        let Ok(head) = self.activity.inbound_head.try_lock() else {
            return;
        };
        let Ok(screen) = player.current_screen_handler.try_lock() else {
            return;
        };
        if head.is_none()
            && player.inbound_packets.is_empty()
            && player.has_client_loaded()
            && !self.inventory_opened.load(Relaxed)
            && std::ptr::addr_eq(
                Arc::as_ptr(&screen),
                Arc::as_ptr(&player.player_screen_handler),
            )
            && let Ok(_handler) = screen.try_lock()
        {
            apply();
        }
    }

    pub(super) fn enqueue_play_input(&self, player: &Arc<Player>, packet: RawPacket) {
        let ready = (Self::is_regional_reply(packet.id)
            || Self::can_prepare_movement(&packet)
            || Self::can_prepare_attack(&packet)
            || Self::can_route_inventory(&packet))
            && self.is_current_protocol();
        player.inbound_packets.push(packet);
        if ready {
            player.world().offer_regional_input(self.id, player);
        }
    }

    pub(crate) fn drain_world_input(
        self: &Arc<Self>,
        player: &Player,
        commands: Option<PlayerCommandIngress<'_>>,
        world_owner: Option<&Arc<World>>,
        budget: usize,
    ) {
        // Keep the region and world from consuming the same connection concurrently.
        let Ok(mut head) = self.activity.inbound_head.try_lock() else {
            return;
        };
        if head.is_none() && player.inbound_packets.is_empty() {
            return;
        }
        let world = player.world();
        let owned = world_owner.is_some_and(|owner| Arc::ptr_eq(owner, &world));
        let Some(player) = world
            .get_player_by_uuid(player.gameprofile.id)
            .filter(|registered| std::ptr::eq(registered.as_ref(), player))
        else {
            return;
        };
        let Some(server) = world.server.upgrade() else {
            return;
        };
        let mut spin_attack_prediction = None;
        for _ in 0..budget {
            let Some(QueuedPlayInput {
                packet,
                mut movement,
                attack,
            }) = head
                .take()
                .or_else(|| player.inbound_packets.pop().map(Into::into))
            else {
                break;
            };
            if self.is_closed() {
                break;
            }
            if let Some(committed) = movement
                .as_mut()
                .and_then(|movement| movement.committed.take())
            {
                player.finish_regional_client_move(&committed);
                continue;
            }
            let original = (movement.is_some() || attack.is_some()).then(|| packet.payload.clone());
            let Some(packet) = self.filter_play_packet(&server, &player, packet) else {
                continue;
            };
            let unchanged = original.is_some_and(|raw| {
                std::ptr::eq(packet.payload.as_ref(), raw.as_ref()) && self.is_current_protocol()
            });
            if let Some(movement) = movement
                && unchanged
                && packet.id == SPlayerAuthInput::PACKET_ID
                && let Some(extent) = owned_extent(&movement.extent, &player)
            {
                tracing::trace!(
                    player = %player.gameprofile.name,
                    input_tick = movement.input.tick.0,
                    "Applying region-prepared Bedrock movement"
                );
                self.apply_player_auth_input(
                    &player,
                    movement.input,
                    &server,
                    &mut spin_attack_prediction,
                    owned.then_some(&extent),
                );
            } else if let Some(attack) = attack
                && unchanged
                && packet.id == SInventoryTransaction::PACKET_ID
                && owned
                && owned_extent(&attack.extent, &player).is_some()
            {
                tracing::trace!(player = %player.gameprofile.name, "Applying region-prepared Bedrock melee attack");
                self.handle_inventory_action(&player, attack.packet);
            } else if let Err(err) = self.handle_queued_play_packet(
                &player,
                &server,
                &packet,
                &mut spin_attack_prediction,
                commands,
            ) {
                tracing::error!("Failed to handle Bedrock play packet: {err}");
            }
        }
        // A trident release can follow its predicted start in the same packet drain.
        self.reconcile_spin_attack_prediction(&player, spin_attack_prediction);
        drop(head);
        // A world-only packet can have held the head while regional work was offered.
        if owned && !player.inbound_packets.is_empty() {
            player.world().offer_regional_input(self.id, &player);
        }
    }

    const fn is_regional_reply(id: i32) -> bool {
        matches!(
            id,
            NetworkStackLatency::PACKET_ID | SClientCacheBlobStatus::PACKET_ID
        )
    }

    const fn can_prepare_movement(packet: &RawPacket) -> bool {
        packet.id == SPlayerAuthInput::PACKET_ID && packet.payload.len() <= REGIONAL_MOVEMENT_BYTES
    }

    fn can_prepare_attack(packet: &RawPacket) -> bool {
        // The simple entity-use transaction has no old request, legacy slots or actions.
        if packet.id != SInventoryTransaction::PACKET_ID
            || packet.payload.len() > REGIONAL_ATTACK_BYTES
        {
            return false;
        }
        let Some(mut data) = packet.payload.strip_prefix(&[0, 0, 3, 0]) else {
            return false;
        };
        VarULong::read(&mut data).is_ok()
            && VarInt::read(&mut data).is_ok_and(|action| action.0 == 1)
    }

    fn prepare_attack(
        player: &Player,
        mut payload: &[u8],
        extent: &RegionChunks,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<PreparedAttack> {
        let packet = SInventoryTransaction::read(&mut payload).ok()?;
        let TransactionData::UseItemOnEntity(data) = &packet.transaction_data else {
            return None;
        };
        if !payload.is_empty() || data.action_type.0 != 1 || data.hand != HandSlot::Mainhand {
            return None;
        }
        let world = player.world();
        let target =
            world.get_entity_by_id(i32::try_from(data.target_entity_runtime_id.0).ok()?)?;
        let base = target.get_entity();
        (!base.is_removed()
            && std::ptr::eq(base.world.load().as_ref(), world.as_ref())
            && owns(base.chunk_pos.load())
            && owns(player.get_entity().chunk_pos.load()))
        .then(|| PreparedAttack {
            extent: Arc::downgrade(extent),
            packet,
        })
    }

    fn prepare_movement(
        player: &Player,
        mut payload: &[u8],
        extent: &RegionChunks,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<PreparedMovement> {
        let input = SPlayerAuthInput::read(&mut payload).ok()?;
        let position = input.position;
        if ![position.x, position.y, position.z]
            .into_iter()
            .all(f32::is_finite)
            || !owns(BlockPos::floored_v(position.to_f64()).chunk_position())
            || !owns(player.get_entity().chunk_pos.load())
        {
            return None;
        }
        Some(PreparedMovement {
            extent: Arc::downgrade(extent),
            input,
            fully_read: payload.is_empty(),
            committed: None,
        })
    }

    fn prepare_regional_move(
        &self,
        player: &Player,
        server: &Server,
        extent: &RegionChunks,
        queued: &mut QueuedPlayInput,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> RegionalMovePreparation {
        if !player.get_entity().on_ground.load(Relaxed) && !player.is_flying() {
            return RegionalMovePreparation::Unprepared;
        }
        let Some(mut prepared) =
            Self::prepare_movement(player, &queued.packet.payload, extent, owns)
        else {
            return RegionalMovePreparation::Unprepared;
        };
        if prepared.fully_read {
            match self.try_accept_regional_auth_move(player, server, extent, &prepared.input) {
                Some(RegionalClientMove::Complete {
                    position_changed,
                    rotation_changed,
                }) => {
                    let message = if position_changed {
                        "Completed grounded Bedrock movement on its region"
                    } else {
                        "Consumed stationary Bedrock input on its region"
                    };
                    tracing::trace!(
                        player = %player.gameprofile.name,
                        rotation_changed,
                        input_tick = prepared.input.tick.0,
                        "{message}"
                    );
                    return RegionalMovePreparation::Consumed;
                }
                Some(RegionalClientMove::Committed(committed)) => {
                    prepared.committed = Some(committed);
                }
                None => {}
            }
        }
        if prepared.committed.is_some() {
            tracing::trace!(
                player = %player.gameprofile.name,
                input_tick = prepared.input.tick.0,
                "Committed Bedrock movement on its region"
            );
        } else {
            tracing::trace!(
                player = %player.gameprofile.name,
                input_tick = prepared.input.tick.0,
                "Prepared Bedrock movement on its region"
            );
        }
        queued.movement = Some(prepared);
        RegionalMovePreparation::Deferred
    }

    pub(crate) fn drain_regional_input(
        &self,
        player: &Player,
        server: &Server,
        extent: &RegionChunks,
        owns: impl Fn(Vector2<i32>) -> bool,
    ) -> bool {
        let Ok(mut head) = self.activity.inbound_head.try_lock() else {
            return true;
        };
        for _ in 0..REGIONAL_PACKET_BUDGET {
            if !owns(player.get_entity().chunk_pos.load()) {
                return true;
            }
            if !self.is_current_protocol()
                || server.plugin_manager.has_handlers::<PacketReceivedEvent>()
                || server.plugin_manager.has_handlers::<PacketSentEvent>()
            {
                return false;
            }
            let was_waiting = head.is_some();
            let Some(mut queued) = head
                .take()
                .or_else(|| player.inbound_packets.pop().map(Into::into))
            else {
                return false;
            };
            // Keep the first world-owned packet ahead of everything behind it.
            if !Self::is_regional_reply(queued.packet.id) {
                let owns_player = || owns(player.get_entity().chunk_pos.load());
                if !was_waiting
                    && self
                        .try_regional_inventory_request(player, server, &queued.packet, owns_player)
                        .is_some()
                {
                    continue;
                }
                if queued
                    .movement
                    .as_ref()
                    .is_some_and(|movement| movement.committed.is_some())
                {
                    *head = Some(queued);
                    return false;
                }
                let movement = Self::can_prepare_movement(&queued.packet);
                if movement
                    && self.try_regional_teleport_input(player, &queued.packet.payload, owns_player)
                {
                    continue;
                }
                if movement && !was_waiting {
                    match self.prepare_regional_move(player, server, extent, &mut queued, &owns) {
                        RegionalMovePreparation::Consumed => continue,
                        RegionalMovePreparation::Deferred => {
                            *head = Some(queued);
                            return false;
                        }
                        RegionalMovePreparation::Unprepared => {}
                    }
                }
                if !was_waiting
                    && Self::can_prepare_attack(&queued.packet)
                    && let Some(attack) =
                        Self::prepare_attack(player, &queued.packet.payload, extent, &owns)
                {
                    tracing::trace!(player = %player.gameprofile.name, "Prepared Bedrock melee attack on its region");
                    queued.attack = Some(attack);
                }
                let payload = (movement && !was_waiting).then(|| queued.packet.payload.clone());
                *head = Some(queued);
                if was_waiting {
                    return false;
                }
                // The world can take the raw packet while the region decodes its copy.
                drop(head);
                if let Some(payload) = payload
                    && let Some(movement) = Self::prepare_movement(player, &payload, extent, &owns)
                    && let Ok(mut head) = self.activity.inbound_head.try_lock()
                    && let Some(pending) = head.as_mut()
                    && pending.packet.id == SPlayerAuthInput::PACKET_ID
                    && std::ptr::eq(pending.packet.payload.as_ref(), payload.as_ref())
                {
                    tracing::trace!(
                        player = %player.gameprofile.name,
                        input_tick = movement.input.tick.0,
                        "Prepared Bedrock movement on its region"
                    );
                    pending.movement = Some(movement);
                }
                return false;
            }
            let packet = queued.packet;
            let reader = &mut packet.payload.as_ref();
            let result = if packet.id == NetworkStackLatency::PACKET_ID {
                NetworkStackLatency::read(reader)
                    .map(|packet| self.apply_network_stack_latency(player, packet, false))
            } else {
                SClientCacheBlobStatus::read(reader)
                    .map(|packet| self.apply_client_cache_blob_status(packet, false))
            };
            if let Err(error) = result {
                tracing::error!(
                    packet_id = packet.id,
                    "Failed to handle Bedrock play packet: {error}"
                );
            }
        }
        !player.inbound_packets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;
    use pumpkin_data::{
        Block, effect::StatusEffect, entity::EntityType, item::Item, item_stack::ItemStack,
        potion::Effect,
    };
    use pumpkin_world::world::BlockFlags;
    use sculith_config::{
        AdvancedConfiguration, BasicConfiguration, telemetry::TelemetryConfig, world::WorldPreset,
    };
    use sculith_protocol::{
        bedrock::{
            client::{CMobEffect, CMovePlayer, update_attributes::CUpdateAttributes},
            network_item::NetworkItemDescriptor,
            server::player_auth_input::InputData,
        },
        codec::{var_int::VarInt, var_uint::VarUInt, var_ulong::VarULong},
        serial::PacketWrite,
    };
    use sculith_util::{GameMode, Hand, math::vector3::Vector3};
    use std::{
        sync::{Mutex, atomic::AtomicBool, mpsc},
        time::{Duration, Instant},
    };

    use crate::{
        data::VanillaData,
        entity::{Entity, interaction::InteractionEntity},
        net::{ClientPlatform, GameProfile, PlayerConfig, packet_limiter::PacketRateLimiter},
        plugin::{EventHandler, api::events::EventPriority},
        server::TickDeadline,
    };

    const TICK_PERIOD: Duration = Duration::from_millis(50);

    #[derive(Default)]
    struct HealthPacketObserver(AtomicBool);

    impl EventHandler<PacketSentEvent> for HealthPacketObserver {
        fn handle_blocking<'a>(
            &'a self,
            _: &'a Arc<Server>,
            event: &'a mut PacketSentEvent,
        ) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                if event.packet_id == CUpdateAttributes::PACKET_ID {
                    assert!(event.player.abilities.try_lock().is_ok());
                    assert_eq!(event.player.get_exhaustion(), 1.0);
                    self.0.store(true, Relaxed);
                }
            })
        }
    }

    struct TeleportPacketObserver {
        delayed_x: f32,
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl EventHandler<PacketSentEvent> for TeleportPacketObserver {
        fn handle_blocking<'a>(
            &'a self,
            _: &'a Arc<Server>,
            event: &'a mut PacketSentEvent,
        ) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                if event.packet_id != CMovePlayer::PACKET_ID {
                    return;
                }
                let mut packet = CMovePlayer::read(&mut event.payload.as_ref()).unwrap();
                if packet.mode != CMovePlayer::MODE_TELEPORT {
                    return;
                }
                if packet.position.x == self.delayed_x {
                    self.entered.send(()).unwrap();
                    self.release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                } else {
                    // A hook can replace the primary frame with an edited teleport.
                    packet.position.x += 0.25;
                    let mut payload = Vec::new();
                    packet.write(&mut payload).unwrap();
                    event.extra_packets.push((event.packet_id, payload.into()));
                    event.cancelled = true;
                }
            })
        }
    }

    fn tick_region(world: &Arc<World>, pos: Vector2<i32>, at: Instant) {
        let layout = world.region_layout.load();
        let owner = layout.owner(pos).unwrap();
        world.advance_region_time(
            owner,
            layout.chunks(owner).unwrap(),
            TickDeadline {
                at,
                period: TICK_PERIOD,
            },
        );
    }

    fn verify_food_finish_retries_after_handoff(
        world: &Arc<World>,
        server: &Server,
        player: &Arc<Player>,
        client: &BedrockClient,
        origin: Vector2<i32>,
        destination: Vector2<i32>,
    ) {
        let position = |chunk: Vector2<i32>| {
            Vector3::new(
                f64::from(chunk.x) * 16.0 + 8.0,
                100.0,
                f64::from(chunk.y) * 16.0 + 8.0,
            )
        };
        let hunger = &player.hunger_manager;
        let living = &player.living_entity;
        let food = || {
            (
                player.get_food_level(),
                player.inventory.held_item().item_count,
            )
        };
        let saved_hunger = (hunger.level.load(), hunger.saturation.load());
        let saved_item = player.inventory.held_item();
        let saved_position = player.get_entity().pos.load();
        let was_loaded = player.has_client_loaded();
        player.set_client_loaded(true);
        player.get_entity().set_pos(position(origin));
        hunger.level.store(10);
        hunger.saturation.store(0.0);
        let apple = ItemStack::new(1, &Item::APPLE);
        player.inventory.set_held_item(apple.clone());
        living.start_bedrock_consumable(server, player, client, Hand::Right, apple);
        assert_eq!(
            client.food_use_slot(),
            Some(player.inventory.get_selected_slot())
        );
        living.item_use_time.store(2, Relaxed);
        let first = Instant::now() + TICK_PERIOD;
        tick_region(world, origin, first);
        assert_eq!(living.item_use_time.load(Relaxed), 1);

        player.get_entity().set_pos(position(destination));
        tick_region(world, destination, first);
        assert_eq!(food(), (10, 1));

        // Replacing the registry leaves the previously indexed generation stale.
        let previous = world.entities.load_full();
        let marker = InteractionEntity::new(Entity::new(
            world.clone(),
            position(destination),
            &EntityType::INTERACTION,
        ));
        let marker_id = marker.get_entity().entity_id;
        let due = first + TICK_PERIOD;
        tick_region(world, origin, due);
        world.entities.store(Arc::new(vec![marker]));
        tick_region(world, destination, due);
        assert_eq!(food(), (10, 1));

        assert!(world.get_entity_by_id(marker_id).is_some());
        tick_region(world, destination, due + TICK_PERIOD);
        assert_eq!(food(), (14, 0));
        tick_region(world, destination, due + TICK_PERIOD * 2);
        assert_eq!(food(), (14, 0));

        world.entities.store(previous);
        player.set_client_loaded(was_loaded);
        player.get_entity().set_pos(saved_position);
        player.inventory.set_held_item(saved_item);
        hunger.level.store(saved_hunger.0);
        hunger.saturation.store(saved_hunger.1);
    }

    fn verify_pickup_timer_handoff(
        world: &Arc<World>,
        player: &Arc<Player>,
        origin: Vector2<i32>,
        destination: Vector2<i32>,
    ) {
        let before_pickup = Instant::now();
        assert!(player.begin_experience_pickup());
        let delay = || *player.experience_pick_up_delay.lock().unwrap();
        tick_region(world, origin, before_pickup);
        assert_eq!(delay(), 2);

        let first = Instant::now() + TICK_PERIOD;
        tick_region(world, origin, first);
        assert_eq!(delay(), 1);
        assert!(!player.begin_experience_pickup());

        let next = first + TICK_PERIOD;
        player.get_entity().set_pos(Vector3::new(8.0, 100.0, 8.0));
        tick_region(world, destination, first);
        assert_eq!(delay(), 1);
        tick_region(world, destination, next);
        assert_eq!(delay(), 0);
        tick_region(world, origin, next);
        assert_eq!(delay(), 0);
        assert!(player.begin_experience_pickup());
    }

    fn verify_item_cooldown_handoff(
        world: &Arc<World>,
        server: &Server,
        player: &Arc<Player>,
        client: &BedrockClient,
        origin: Vector2<i32>,
        destination: Vector2<i32>,
    ) {
        let original_pos = player.get_entity().pos.load();
        let before_start = Instant::now();
        player.start_cooldown("minecraft:ender_pearl".into(), 4);
        player.start_cooldown("shield".into(), 2);
        let remaining = || player.get_item_cooldown("minecraft:ender_pearl");
        tick_region(world, origin, before_start);
        assert_eq!(remaining(), Some(4));

        let first = Instant::now() + TICK_PERIOD;
        tick_region(world, origin, first);
        tick_region(world, origin, first);
        assert_eq!(remaining(), Some(3));
        assert_eq!(player.get_cooldown("minecraft:ender_pearl"), 0.75);
        assert_eq!(player.get_item_cooldown("shield"), Some(1));

        player.get_entity().set_pos(Vector3::new(520.0, 100.0, 8.0));
        let next = first + TICK_PERIOD;
        tick_region(world, origin, next);
        tick_region(world, destination, first);
        assert_eq!(remaining(), Some(3));
        tick_region(world, destination, next);
        assert_eq!(remaining(), Some(2));
        assert!(!player.is_on_cooldown("shield"));

        client.tick_item_cooldowns(server, player, None);
        assert_eq!(remaining(), Some(1));
        client.tick_item_cooldowns(server, player, None);
        assert_eq!(remaining(), None);
        assert!(!player.is_on_cooldown("minecraft:ender_pearl"));
        player.get_entity().set_pos(original_pos);
    }

    fn verify_status_effect_handoff(
        world: &Arc<World>,
        server: &Server,
        player: &Arc<Player>,
        regions: [Vector2<i32>; 2],
        refresh: impl Fn(bool),
    ) {
        let [west, east] = regions;
        let original_pos = player.get_entity().pos.load();
        let add = |effect_type, duration| {
            player.add_effect(Effect {
                effect_type,
                duration,
                amplifier: 0,
                ambient: false,
                show_particles: effect_type == &StatusEffect::NIGHT_VISION,
                show_icon: true,
                blend: false,
            });
        };
        let duration = |effect_type| player.get_effect(effect_type).map(|effect| effect.duration);
        let night_vision = &StatusEffect::NIGHT_VISION;
        let speed = &StatusEffect::SPEED;
        add(night_vision, 3);
        add(speed, -1);

        refresh(true);
        let first = Instant::now() + TICK_PERIOD;
        tick_region(world, west, first);
        tick_region(world, west, first);
        assert_eq!(duration(night_vision), Some(2));
        assert_eq!(duration(speed), Some(-1));

        refresh(false);
        player.get_entity().set_pos(Vector3::new(520.0, 100.0, 8.0));
        let next = first + TICK_PERIOD;
        tick_region(world, west, next);
        tick_region(world, east, first);
        assert_eq!(duration(night_vision), Some(2));
        tick_region(world, east, next);
        assert_eq!(duration(night_vision), Some(1));
        player.tick_with_deadline(
            server,
            Some(TickDeadline {
                at: next,
                period: TICK_PERIOD,
            }),
        );
        assert_eq!(duration(night_vision), Some(1));
        tick_region(world, east, next + TICK_PERIOD);
        assert_eq!(duration(night_vision), Some(0));
        let expires = next + TICK_PERIOD * 2;
        tick_region(world, east, expires);
        assert_eq!(duration(night_vision), None);
        tick_region(world, east, expires);
        player.tick_with_deadline(
            server,
            Some(TickDeadline {
                at: expires,
                period: TICK_PERIOD,
            }),
        );
        let client = player.client.bedrock().unwrap();
        let mut outgoing = client.outgoing_packet_queue_recv.try_lock().unwrap();
        let receiver = outgoing.as_mut().unwrap();
        let removals = std::iter::from_fn(|| receiver.try_recv().ok())
            .filter(|outgoing| {
                let (id, payload) = super::super::split_raw_packet(&outgoing.data).unwrap();
                if i32::from(id) != CMobEffect::PACKET_ID {
                    return false;
                }
                let body = &mut payload.as_ref();
                VarULong::read(body).unwrap().0 == player.entity_id() as u64
                    && u8::read(body).unwrap() == CMobEffect::EVENT_REMOVE
                    && VarInt::read(body).unwrap().0 == 16
            })
            .count();
        assert_eq!(removals, 1);
        drop(outgoing);

        let poison = &StatusEffect::POISON;
        add(night_vision, 3);
        add(poison, 40);
        tick_region(world, east, next + TICK_PERIOD * 3);
        assert_eq!(duration(night_vision), Some(3));
        assert_eq!(duration(poison), Some(40));
        player.tick_with_deadline(server, None);
        assert_eq!(duration(night_vision), Some(2));
        assert_eq!(duration(poison), Some(39));
        player.remove_all_effects();
        player.get_entity().set_pos(original_pos);
    }

    async fn verify_mining_clock(
        world: &Arc<World>,
        server: &Server,
        player: &Arc<Player>,
        client: &BedrockClient,
        chunk: Vector2<i32>,
    ) {
        let terrain = world.level.read_chunk_sync(&chunk, Arc::clone).unwrap();
        let target = BlockPos::new(9, 100, 8);
        let dirt = Block::DIRT.default_state.id;
        let stone = Block::STONE.default_state.id;
        let set_target = |state| terrain.set_block_absolute_y(9, 100, 8, state);
        set_target(dirt);
        player.mining.store(true, Relaxed);
        *player.mining_pos.lock().unwrap() = target;
        let at = Instant::now();
        let step = TickDeadline {
            at,
            period: TICK_PERIOD,
        };
        assert_eq!(
            client.tick_mining_clock(server, player, world, target, dirt, Some(step)),
            1
        );
        // Background chunk work can temporarily prevent the region from reading
        // the block. Keep the deadline fixed while waiting for that work.
        let advance_mining = |deadline, expected| async move {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    tick_region(world, chunk, deadline);
                    if client.mining_prediction_ticks(server, player, world, target, dirt, 1)
                        == expected
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("regional mining should finish once the chunk can be read");
        };
        advance_mining(at + TICK_PERIOD, 2).await;
        tick_region(world, chunk, at + TICK_PERIOD);
        assert_eq!(
            client.mining_prediction_ticks(server, player, world, target, dirt, 1),
            2
        );
        player.inbound_packets.push(RawPacket {
            id: -1,
            payload: bytes::Bytes::new(),
        });
        let repeated = TickDeadline {
            at: at + TICK_PERIOD,
            period: TICK_PERIOD,
        };
        assert_eq!(
            client.tick_mining_clock(server, player, world, target, dirt, Some(repeated)),
            1
        );
        assert!(player.inbound_packets.pop().is_some());
        assert_eq!(
            client.tick_mining_clock(server, player, world, target, dirt, Some(repeated)),
            2
        );

        set_target(stone);
        advance_mining(at + TICK_PERIOD * 2, 1).await;
        assert_eq!(
            client.mining_prediction_ticks(server, player, world, target, dirt, 1),
            1
        );
        assert_eq!(
            client.mining_prediction_ticks(server, player, world, target, stone, 1),
            1
        );
        player.reset_mining();
    }

    fn verify_chunk_delivery_clock(client: &BedrockClient) {
        assert!(!client.chunk_view_published());
        let previous = client.begin_chunk_view();
        let current = client.begin_chunk_view();
        client.publish_chunk_view(previous);
        assert!(!client.chunk_view_published());
        client.publish_chunk_view(current);
        client.publish_chunk_view(previous);
        assert!(client.chunk_view_published());

        let at = Instant::now();
        let step = TickDeadline {
            at,
            period: TICK_PERIOD,
        };
        assert!(client.claim_chunk_step(Some(step)));
        assert!(!client.claim_chunk_step(Some(step)));
        assert!(client.claim_chunk_step(Some(TickDeadline {
            at: at + TICK_PERIOD,
            ..step
        })));
    }

    fn verify_attack_preparation(
        world: &Arc<World>,
        player: &Arc<Player>,
        client: &BedrockClient,
        west: Vector2<i32>,
    ) {
        let target = crate::entity::r#type::from_type(
            &pumpkin_data::entity::EntityType::PIG,
            Vector3::new(520.0, 100.0, 8.0),
            world,
            uuid::Uuid::new_v4(),
        );
        world.add_entity_silent(target.clone());
        let base = target.get_entity();
        let mut payload = vec![0, 0, 3, 0];
        VarULong(base.entity_id as u64).write(&mut payload).unwrap();
        VarInt(1).write(&mut payload).unwrap();
        VarInt(0).write(&mut payload).unwrap();
        payload.push(0);
        NetworkItemDescriptor::default()
            .write(&mut payload)
            .unwrap();
        payload.extend_from_slice(&[0; 24]);
        let payload: bytes::Bytes = payload.into();
        let advance = |expected| {
            client.enqueue_play_input(
                player,
                RawPacket {
                    id: SInventoryTransaction::PACKET_ID,
                    payload: payload.clone(),
                },
            );
            tick_region(world, west, Instant::now());
            let head = client.activity.inbound_head.lock().unwrap().take().unwrap();
            assert!(std::ptr::eq(head.packet.payload.as_ref(), payload.as_ref()));
            assert_eq!(head.attack.is_some(), expected);
            assert!(player.inbound_packets.is_empty());
        };
        advance(false);
        base.set_pos(Vector3::new(9.0, 100.0, 8.0));
        advance(true);
        world.remove_entity(target.as_ref());
    }

    fn queue_auth_input(
        client: &BedrockClient,
        player: &Arc<Player>,
        position: Vector3<f32>,
        input_tick: u64,
        yaw: f32,
        flags: &[InputData],
    ) {
        let mut payload = Vec::new();
        for value in [0.0, yaw, position.x, position.y, position.z, 0.0, 0.0, yaw] {
            value.write(&mut payload).unwrap();
        }
        VarUInt(u32::try_from(flags.len()).unwrap())
            .write(&mut payload)
            .unwrap();
        for &flag in flags {
            VarInt(flag as i32).write(&mut payload).unwrap();
        }
        for _ in 0..2 {
            VarUInt(0).write(&mut payload).unwrap();
        }
        VarInt(0).write(&mut payload).unwrap();
        payload.extend([0; 8]);
        VarULong(input_tick).write(&mut payload).unwrap();
        let on_ground = flags.contains(&InputData::VerticalCollision);
        for delta in [0.0f32, if on_ground { -0.08 } else { 0.0 }, 0.0] {
            delta.write(&mut payload).unwrap();
        }
        payload.extend([0; 5]);
        payload.extend([0; 28]);
        client.enqueue_play_input(
            player,
            RawPacket {
                id: SPlayerAuthInput::PACKET_ID,
                payload: payload.into(),
            },
        );
    }

    fn verify_stationary_inputs(
        world: &Arc<World>,
        player: &Arc<Player>,
        client: &Arc<BedrockClient>,
        chunk: Vector2<i32>,
    ) {
        let entity = player.get_entity();
        let position = player.bedrock_pos().to_f32_lossy();
        entity.set_pos(player.feet_from_bedrock_pos(position.to_f64()));
        entity.on_ground.store(true, Relaxed);
        player.set_client_loaded(true);
        client.client_delta.store(Vector3::new(0.0, -0.08, 0.0));

        let grounded = [InputData::VerticalCollision];
        let enqueue = |tick| queue_auth_input(client, player, position, tick, 0.0, &grounded);
        let start = 200;
        let end = start + REGIONAL_PACKET_BUDGET as u64;
        let at = Instant::now() + TICK_PERIOD * 10;
        for tick in start..=end {
            enqueue(tick);
        }
        tick_region(world, chunk, at);
        assert_eq!(client.input_tick.load(Relaxed), end - 1);
        assert_eq!(player.inbound_packets.len(), 1);
        assert!(client.activity.inbound_head.lock().unwrap().is_none());
        tick_region(world, chunk, at + TICK_PERIOD);
        assert_eq!(client.input_tick.load(Relaxed), end);
        assert!(player.inbound_packets.is_empty());

        // An otherwise identical landing still needs the world's fall processing.
        player.living_entity.fall_distance.store(1.0);
        enqueue(end + 1);
        enqueue(end + 2);
        tick_region(world, chunk, at + TICK_PERIOD * 2);
        assert_eq!(client.input_tick.load(Relaxed), end);
        assert_eq!(player.inbound_packets.len(), 1);
        assert!(client.activity.inbound_head.lock().unwrap().is_some());
        assert_eq!(player.living_entity.fall_distance.load(), 1.0);
        client.drain_world_input(player, None, Some(world), 1);
        assert_eq!(client.input_tick.load(Relaxed), end + 1);
        assert_eq!(player.living_entity.fall_distance.load(), 0.0);
        tick_region(world, chunk, at + TICK_PERIOD * 3);
        assert_eq!(client.input_tick.load(Relaxed), end + 2);
        assert!(player.inbound_packets.is_empty());

        // Flying descent can change the control state without a StartSneaking flag.
        player.abilities.lock().unwrap().flying = true;
        for (offset, held) in [(0, InputData::Descend), (2, InputData::SneakDown)] {
            let tick = end + 3 + u64::from(offset);
            let deadline = at + TICK_PERIOD * (4 + offset);
            queue_auth_input(
                client,
                player,
                position,
                tick,
                0.0,
                &[InputData::VerticalCollision, held],
            );
            tick_region(world, chunk, deadline);
            assert_eq!(client.input_tick.load(Relaxed), tick - 1);
            assert!(!entity.is_sneaking());
            client.drain_world_input(player, None, Some(world), 1);
            assert_eq!(client.input_tick.load(Relaxed), tick);
            assert!(entity.is_sneaking());

            enqueue(tick + 1);
            client.drain_world_input(player, None, Some(world), 1);
            assert!(!entity.is_sneaking());
        }
        player.abilities.lock().unwrap().flying = false;
    }

    fn verify_stationary_turns(
        world: &Arc<World>,
        player: &Arc<Player>,
        client: &Arc<BedrockClient>,
        chunk: Vector2<i32>,
    ) {
        let entity = player.get_entity();
        let position = player.bedrock_pos().to_f32_lossy();
        let start = client.input_tick.load(Relaxed) + 1;
        let enqueue = |tick, yaw| {
            queue_auth_input(
                client,
                player,
                position,
                tick,
                yaw,
                &[InputData::VerticalCollision],
            );
        };
        let at = Instant::now() + TICK_PERIOD * 20;
        enqueue(start, 22.5);
        enqueue(start + 1, 45.0);
        tick_region(world, chunk, at);
        assert_eq!(entity.yaw.load(), 22.5);
        assert!(client.activity.inbound_head.lock().unwrap().is_some());

        // Without a tracking entry, finishing delivery still needs the world.
        client.drain_world_input(player, None, Some(world), 1);
        world
            .entity_tracker
            .add_entity(&(player.clone() as Arc<dyn EntityBase>), world);
        tick_region(world, chunk, at + TICK_PERIOD);
        assert_eq!(entity.yaw.load(), 45.0);
        assert_eq!(client.input_tick.load(Relaxed), start + 1);
        assert!(client.activity.inbound_head.lock().unwrap().is_none());

        player.living_entity.fall_distance.store(1.0);
        enqueue(start + 2, 67.5);
        enqueue(start + 3, 90.0);
        tick_region(world, chunk, at + TICK_PERIOD * 2);
        assert_eq!(entity.yaw.load(), 67.5);
        assert_eq!(player.living_entity.fall_distance.load(), 1.0);
        assert!(client.activity.inbound_head.lock().unwrap().is_some());
        client.drain_world_input(player, None, Some(world), 1);
        assert_eq!(player.living_entity.fall_distance.load(), 0.0);
        tick_region(world, chunk, at + TICK_PERIOD * 3);
        assert_eq!(entity.yaw.load(), 90.0);
        assert_eq!(client.input_tick.load(Relaxed), start + 3);
        assert!(player.inbound_packets.is_empty());
        assert!(client.activity.inbound_head.lock().unwrap().is_none());
        world.entity_tracker.remove_entity(player.as_ref(), world);
    }

    fn verify_teleport_acknowledgement(
        world: &Arc<World>,
        player: &Arc<Player>,
        client: &Arc<BedrockClient>,
        chunk: Vector2<i32>,
    ) {
        let enqueue = |position| queue_auth_input(client, player, position, 7, 0.0, &[]);
        let advance = || tick_region(world, chunk, Instant::now());
        let previous_input = client.input_tick.load(Relaxed);

        enqueue(player.bedrock_pos().to_f32_lossy());
        advance();
        assert!(client.activity.inbound_head.lock().unwrap().is_some());

        let mut destination = player.position();
        destination.x += 1.0;
        player.request_teleport(destination, 0.0, 0.0);
        player.set_client_loaded(false);
        enqueue(client.teleport_sent_pos.load());
        advance();
        assert!(player.awaiting_teleport.lock().unwrap().is_none());
        assert!(client.activity.inbound_head.lock().unwrap().is_none());
        assert_eq!(client.teleport_unconfirmed_inputs.load(Relaxed), 1);
        assert_eq!(client.input_tick.load(Relaxed), previous_input);
        assert_eq!(player.get_entity().pos.load(), destination);
        assert!(player.has_client_loaded());

        player.request_teleport(Vector3::new(10.0, 100.0, 8.0), 0.0, 0.0);
        let sent_id = client.activity.sent_teleport_id.load().unwrap();
        // Start a newer teleport before its packet has been queued.
        let next_id = sent_id.wrapping_add(1);
        let next = (VarInt(next_id), destination);
        *player.awaiting_teleport.lock().unwrap() = Some(next);
        player.set_client_loaded(false);
        enqueue(client.teleport_sent_pos.load());
        advance();
        assert_eq!(*player.awaiting_teleport.lock().unwrap(), Some(next));
        assert!(client.activity.inbound_head.lock().unwrap().is_some());
        client.drain_world_input(player, None, Some(world), 1);
        assert!(client.activity.inbound_head.lock().unwrap().is_none());
        assert_eq!(*player.awaiting_teleport.lock().unwrap(), Some(next));
        assert!(!player.has_client_loaded());

        client.send_teleport(player);
        assert_eq!(client.activity.sent_teleport_id.load(), Some(sent_id));
        player.get_entity().set_pos(destination);
        client.send_teleport(player);
        assert_eq!(client.activity.sent_teleport_id.load(), Some(next_id));
        enqueue(client.teleport_sent_pos.load());
        advance();
        assert!(player.awaiting_teleport.lock().unwrap().is_none());
        assert!(player.has_client_loaded());
    }

    fn verify_superseded_teleport(
        world: &Arc<World>,
        server: &Arc<Server>,
        player: &Arc<Player>,
        client: &Arc<BedrockClient>,
    ) {
        let old = Vector3::new(1034.0, 100.0, 8.0);
        let latest = Vector3::new(1036.0, 100.0, 8.0);
        let (entered, parked) = mpsc::channel();
        let (resume, release) = mpsc::channel();
        server.plugin_manager.register(
            Arc::new(TeleportPacketObserver {
                delayed_x: 1034.0,
                entered,
                release: Mutex::new(release),
            }),
            EventPriority::Normal,
            true,
        );
        let mut outgoing = client.outgoing_packet_queue_recv.try_lock().unwrap();
        let outgoing = outgoing.as_mut().unwrap();
        while outgoing.try_recv().is_ok() {}
        std::thread::scope(|scope| {
            let older = scope.spawn(|| player.request_teleport(old, 0.0, 0.0));
            parked.recv_timeout(Duration::from_secs(10)).unwrap();
            let newer = scope.spawn(|| player.request_teleport(latest, 0.0, 0.0));
            newer.join().unwrap();
            resume.send(()).unwrap();
            older.join().unwrap();
        });

        let sent: Vec<_> = std::iter::from_fn(|| outgoing.try_recv().ok())
            .filter_map(|outgoing| {
                let (id, payload) = super::super::split_raw_packet(&outgoing.data)?;
                (i32::from(id) == CMovePlayer::PACKET_ID)
                    .then(|| CMovePlayer::read(&mut payload.as_ref()).unwrap().position)
            })
            .collect();
        assert_eq!(player.position(), latest);
        let wire = player.bedrock_pos().to_f32_lossy().add_raw(0.25, 0.0, 0.0);
        assert_eq!(sent, [wire]);

        player.set_client_loaded(false);
        queue_auth_input(client, player, wire, 8, 0.0, &[InputData::HandledTeleport]);
        client.drain_world_input(player, None, Some(world), 1);
        assert!(player.awaiting_teleport.lock().unwrap().is_none());
        assert!(player.has_client_loaded());
    }

    fn verify_grounded_walks(
        world: &Arc<World>,
        player: &Arc<Player>,
        client: &Arc<BedrockClient>,
        chunk: Vector2<i32>,
    ) {
        use crate::entity::player::statistics::CustomStatistic;
        use crate::world::chunker;

        let entity = player.get_entity();
        let position = player.bedrock_pos().to_f32_lossy();
        for x in 7..=9 {
            for z in 7..=9 {
                world.set_block_state(
                    &BlockPos::new(x, 99, z),
                    Block::STONE.default_state.id,
                    BlockFlags::FORCE_STATE,
                );
            }
        }
        world.set_block_state(
            &BlockPos::new(9, 100, 8),
            Block::AIR.default_state.id,
            BlockFlags::FORCE_STATE,
        );
        entity.set_pos(player.feet_from_bedrock_pos(position.to_f64()));
        world
            .entity_tracker
            .add_entity(&(player.clone() as Arc<dyn EntityBase>), world);
        chunker::update_position(player);

        let walk = |x, tick| {
            queue_auth_input(
                client,
                player,
                Vector3::new(x, position.y, position.z),
                tick,
                entity.yaw.load(),
                &[InputData::VerticalCollision],
            );
        };
        let distance = || player.get_custom_stat(CustomStatistic::WalkOneCm);
        let at = Instant::now() + TICK_PERIOD * 30;
        let start = client.input_tick.load(Relaxed) + 1;
        walk(8.25, start);
        walk(8.5, start + 1);
        tick_region(world, chunk, at);
        assert_eq!(entity.pos.load().x, 8.5);
        assert_eq!(distance(), 50);
        assert_eq!(entity.movement.load(), Vector3::new(0.25, 0.0, 0.0));
        assert_eq!(entity.move_dist.load(), 0.30);
        assert_eq!(entity.fly_dist.load(), 0.30);
        assert!(client.activity.inbound_head.lock().unwrap().is_none());

        // A step crossing the sound threshold applies its effects once through the world.
        entity.next_step.store(0.38);
        walk(8.75, start + 2);
        walk(8.875, start + 3);
        tick_region(world, chunk, at + TICK_PERIOD);
        assert_eq!(entity.pos.load().x, 8.75);
        assert_eq!(distance(), 50);
        assert!(client.activity.inbound_head.lock().unwrap().is_some());
        client.drain_world_input(player, None, Some(world), 1);
        assert_eq!(distance(), 75);
        assert!(entity.next_step.load() > 0.38);
        tick_region(world, chunk, at + TICK_PERIOD * 2);
        assert_eq!(entity.pos.load().x, 8.875);
        assert_eq!(distance(), 87);
        assert!(player.inbound_packets.is_empty());
        assert!(client.activity.inbound_head.lock().unwrap().is_none());
        verify_grounded_sprints(world, player, client, chunk);
        world.entity_tracker.remove_entity(player.as_ref(), world);
    }

    fn verify_grounded_sprints(
        world: &Arc<World>,
        player: &Arc<Player>,
        client: &Arc<BedrockClient>,
        chunk: Vector2<i32>,
    ) {
        use crate::entity::player::statistics::CustomStatistic;

        let entity = player.get_entity();
        let position = player.bedrock_pos().to_f32_lossy();
        let at = Instant::now() + TICK_PERIOD * 40;
        let start = client.input_tick.load(Relaxed) + 1;
        let grounded_sprint = [InputData::VerticalCollision, InputData::SprintDown];
        queue_auth_input(
            client,
            player,
            position,
            start,
            entity.yaw.load(),
            &[
                InputData::VerticalCollision,
                InputData::SprintDown,
                InputData::StartSprinting,
            ],
        );
        client.drain_world_input(player, None, Some(world), 1);
        assert!(entity.is_sprinting());
        player.set_exhaustion(4.0);

        let sprint = |x, tick| {
            queue_auth_input(
                client,
                player,
                Vector3::new(x, position.y, position.z),
                tick,
                entity.yaw.load(),
                &grounded_sprint,
            );
        };
        let distance = || player.get_custom_stat(CustomStatistic::SprintOneCm);
        let check_exhaustion = |expected: f32| {
            assert!((player.get_exhaustion() - expected).abs() < 0.000_001);
        };
        sprint(9.125, start + 1);
        sprint(9.375, start + 2);
        tick_region(world, chunk, at);
        assert_eq!(entity.pos.load().x, 9.375);
        assert_eq!(distance(), 50);
        assert_eq!(player.get_custom_stat(CustomStatistic::WalkOneCm), 87);
        assert!(client.activity.inbound_head.lock().unwrap().is_none());
        // The same regional tick spends the exhaustion recorded by these moves.
        assert_eq!(player.hunger_manager.saturation.load(), 4.0);
        check_exhaustion(0.05);

        player.hunger_manager.tick(player);
        assert_eq!(player.hunger_manager.saturation.load(), 4.0);
        check_exhaustion(0.05);

        // A region does not wait for the hunger state or apply the sprint twice.
        let hunger = player.abilities.lock().unwrap();
        sprint(9.5, start + 3);
        tick_region(world, chunk, at + TICK_PERIOD);
        assert_eq!(entity.pos.load().x, 9.5);
        assert_eq!(distance(), 50);
        check_exhaustion(0.05);
        assert!(client.activity.inbound_head.lock().unwrap().is_some());
        drop(hunger);
        client.drain_world_input(player, None, Some(world), 1);
        assert_eq!(distance(), 62);
        check_exhaustion(0.063);
        assert!(client.activity.inbound_head.lock().unwrap().is_none());

        queue_auth_input(
            client,
            player,
            player.bedrock_pos().to_f32_lossy(),
            start + 4,
            entity.yaw.load(),
            &[InputData::VerticalCollision, InputData::StopSprinting],
        );
        client.drain_world_input(player, None, Some(world), 1);
        assert!(!entity.is_sprinting());
    }

    fn verify_grounded_chunk_crossings(
        world: &Arc<World>,
        player: &Arc<Player>,
        client: &Arc<BedrockClient>,
        origin: Vector2<i32>,
    ) {
        use crate::entity::player::statistics::CustomStatistic;
        use pumpkin_world::chunk::ChunkData;

        let destination = Vector2::new(origin.x + 1, origin.y);
        world
            .level
            .loaded_chunks
            .entry(destination)
            .or_insert_with(|| ChunkData::empty_sync(destination.x, destination.y));
        world.forced_chunks.lock().unwrap().insert(destination);
        world.update_active_chunks();
        world.tick_chunks();
        assert_eq!(
            world.region_layout.load().owner(origin),
            world.region_layout.load().owner(destination)
        );
        for x in 14..=17 {
            for z in 7..=9 {
                world.set_block_state(
                    &BlockPos::new(x, 99, z),
                    Block::STONE.default_state.id,
                    BlockFlags::FORCE_STATE,
                );
            }
        }
        let entity = player.get_entity();
        entity.set_pos(Vector3::new(15.75, 100.0, 8.0));
        let position = player.bedrock_pos().to_f32_lossy();
        entity.set_pos(player.feet_from_bedrock_pos(position.to_f64()));
        entity.next_step.store(10.0);
        let mut config = (*player.config.load_full()).clone();
        config.view_distance = std::num::NonZeroU8::new(4).unwrap();
        player.config.store(Arc::new(config));
        world
            .entity_tracker
            .add_entity(&(player.clone() as Arc<dyn EntityBase>), world);
        crate::world::chunker::update_position(player);
        let view = player.watched_section.load();
        let simulation_chunk = || {
            player
                .held_chunk_tickets
                .lock()
                .unwrap()
                .simulation
                .unwrap()
                .0
        };
        assert_eq!(simulation_chunk(), origin);
        player
            .chunk_sender
            .lock()
            .unwrap()
            .mark_sent_out_of_band(destination);

        let enqueue = |x, tick| {
            queue_auth_input(
                client,
                player,
                Vector3::new(x, position.y, position.z),
                tick,
                entity.yaw.load(),
                &[InputData::VerticalCollision],
            );
        };
        let at = Instant::now() + TICK_PERIOD * 45;
        let start = client.input_tick.load(Relaxed) + 1;
        let distance = || player.get_custom_stat(CustomStatistic::WalkOneCm);
        let before = distance();
        enqueue(16.125, start);
        enqueue(16.375, start + 1);
        tick_region(world, origin, at);
        assert_eq!(entity.pos.load().x, 16.375);
        assert!(player.watched_section.load() == view);
        assert_eq!(simulation_chunk(), destination);
        assert_eq!(distance() - before, 62);
        assert!(client.activity.inbound_head.lock().unwrap().is_none());

        // When the client still needs the destination chunk, the world keeps
        // finalizing the move and shifting its terrain interests in FIFO.
        assert!(!player.chunk_sender.lock().unwrap().is_chunk_ready(&origin));
        enqueue(15.875, start + 2);
        tick_region(world, destination, at + TICK_PERIOD);
        assert_eq!(entity.pos.load().x, 15.875);
        assert_eq!(simulation_chunk(), destination);
        assert_eq!(distance() - before, 62);
        assert!(client.activity.inbound_head.lock().unwrap().is_some());
        client.drain_world_input(player, None, Some(world), 1);
        assert_eq!(simulation_chunk(), origin);
        assert_eq!(distance() - before, 112);
        world.entity_tracker.remove_entity(player.as_ref(), world);
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "keeps each ownership transition in one session"
    )]
    async fn pending_input_follows_a_merge_split_and_player_crossing() {
        let directory = tempfile::tempdir().unwrap();
        let basic = BasicConfiguration {
            default_level_name: directory
                .path()
                .join("world")
                .to_string_lossy()
                .into_owned(),
            allow_chat_reports: false,
            allow_nether: false,
            allow_end: false,
            ..Default::default()
        };
        let mut advanced = AdvancedConfiguration::default();
        advanced.world.generator = WorldPreset::Flat;
        advanced.networking.bedrock.online_mode = false;
        advanced.advancement.enabled = false;
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
        let address = "127.0.0.1:0".parse().unwrap();
        let client = Arc::new(BedrockClient::new(
            1,
            super::super::NetherNetSession::test_session(address).await,
            address,
            Arc::default(),
            PacketRateLimiter::new(false, 0.0, 0.0),
            Arc::downgrade(&server),
        ));
        let player = Arc::new(Player::new(
            Arc::new(ClientPlatform::Bedrock(client.clone())),
            GameProfile {
                id: uuid::Uuid::new_v4(),
                name: "RegionalInput".into(),
                properties: arc_swap::ArcSwap::default(),
                profile_actions: None,
            },
            PlayerConfig::default(),
            &world,
            GameMode::Survival,
        ));
        player.initial_spawn_complete.store(true, Relaxed);
        world.players.store(Arc::new(vec![player.clone()]));
        let west = Vector2::new(0, 0);
        let east = Vector2::new(32, 0);
        // Preload the bridge fixture so generation cannot interrupt an input
        // that needs to read the current terrain without blocking.
        for x in west.x..=east.x {
            world
                .level
                .loaded_chunks
                .entry(Vector2::new(x, 0))
                .or_insert_with(|| pumpkin_world::chunk::ChunkData::empty_sync(x, 0));
        }
        let refresh = |joined| {
            let mut forced = world.forced_chunks.lock().unwrap();
            forced.clear();
            forced.extend([west, east]);
            if joined {
                forced.extend((1..east.x).map(|x| Vector2::new(x, 0)));
            }
            drop(forced);
            world.update_active_chunks();
            world.tick_chunks();
            let layout = world.region_layout.load();
            assert_eq!(layout.owner(west) == layout.owner(east), joined);
        };
        let advance = |pos| tick_region(&world, pos, Instant::now());
        let offer = |timestamp| {
            player.ping.store(0, Relaxed);
            client.send_latency_probe();
            let mut payload = Vec::new();
            NetworkStackLatency {
                timestamp,
                from_server: true,
            }
            .write(&mut payload)
            .unwrap();
            client.enqueue_play_input(
                &player,
                RawPacket {
                    id: NetworkStackLatency::PACKET_ID,
                    payload: payload.into(),
                },
            );
        };
        let check = || {
            assert!(player.inbound_packets.is_empty());
            assert!(player.ping.load(Relaxed) > 0);
        };

        refresh(false);
        offer(1);
        refresh(true);
        advance(west);
        check();

        offer(2);
        refresh(false);
        advance(west);
        check();

        offer(3);
        player.get_entity().set_pos(Vector3::new(520.0, 100.0, 8.0));
        assert_eq!(player.inbound_packets.len(), 1);
        let pending_during_move = {
            let _moving = client.regional_work().lock().unwrap();
            advance(east);
            player.inbound_packets.len()
        };
        assert_eq!(pending_during_move, 1);
        let pending_with_lock = {
            let _draining = client.activity.inbound_head.lock().unwrap();
            advance(east);
            player.inbound_packets.len()
        };
        assert_eq!(pending_with_lock, 1);
        advance(east);
        check();
        verify_food_finish_retries_after_handoff(&world, &server, &player, &client, west, east);
        verify_pickup_timer_handoff(&world, &player, east, west);
        verify_item_cooldown_handoff(&world, &server, &player, &client, west, east);
        verify_mining_clock(&world, &server, &player, &client, west).await;
        verify_attack_preparation(&world, &player, &client, west);
        verify_stationary_inputs(&world, &player, &client, west);
        verify_stationary_turns(&world, &player, &client, west);
        verify_chunk_delivery_clock(&client);
        verify_grounded_walks(&world, &player, &client, west);
        verify_grounded_chunk_crossings(&world, &player, &client, west);
        verify_teleport_acknowledgement(&world, &player, &client, west);
        verify_status_effect_handoff(&world, &server, &player, [west, east], refresh);

        let waiting = Vector2::new(64, 0);
        assert!(world.region_layout.load().owner(waiting).is_none());
        offer(4);
        player
            .get_entity()
            .set_pos(Vector3::new(1032.0, 100.0, 8.0));
        // Take any former owner's work while the destination has no owner.
        advance(west);
        offer(5);
        player.start_cooldown("shield".into(), 2);
        assert_eq!(player.inbound_packets.len(), 2);
        assert_eq!(player.get_item_cooldown("shield"), Some(2));

        refresh(false);
        let first = Instant::now() + TICK_PERIOD;
        tick_region(&world, waiting, first);
        check();
        assert_eq!(player.get_item_cooldown("shield"), Some(1));
        tick_region(&world, waiting, first);
        assert_eq!(player.get_item_cooldown("shield"), Some(1));
        tick_region(&world, waiting, first + TICK_PERIOD);
        assert_eq!(player.get_item_cooldown("shield"), None);

        let before_freeze = world.region_layout.load_full();
        let owner = before_freeze.owner(waiting).unwrap();
        world.invalidate_region_layout();
        offer(6);
        assert_eq!(player.inbound_packets.len(), 1);

        refresh(false);
        let resumed = world.region_layout.load_full();
        assert_eq!(resumed.owner(waiting), Some(owner));
        assert!(!Arc::ptr_eq(
            before_freeze.chunks(owner).unwrap(),
            resumed.chunks(owner).unwrap()
        ));
        advance(waiting);
        check();

        let observer = Arc::new(HealthPacketObserver::default());
        server
            .plugin_manager
            .register(observer.clone(), EventPriority::Normal, true);
        client.set_player(player.clone());
        verify_superseded_teleport(&world, &server, &player, &client);
        std::thread::scope(|scope| scope.spawn(|| player.set_exhaustion(1.0)).join().unwrap());
        client.clear_player();
        assert!(observer.0.load(Relaxed));

        world.players.store(Arc::default());
        client.close().await;
        world.level.shutdown().await;
    }
}
