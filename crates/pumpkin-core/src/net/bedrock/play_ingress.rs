use std::sync::{Arc, Weak};

use pumpkin_protocol::{
    RawPacket,
    bedrock::{
        network_stack_latency::NetworkStackLatency,
        server::{
            client_cache_blob_status::SClientCacheBlobStatus, player_auth_input::SPlayerAuthInput,
        },
    },
    packet::Packet,
    serial::PacketRead,
};
use pumpkin_util::math::{position::BlockPos, vector2::Vector2};

use crate::{
    entity::{EntityBase, player::Player},
    plugin::server::packet::{PacketReceivedEvent, PacketSentEvent},
    server::{Server, player_commands::PlayerCommandIngress},
    world::{RegionChunks, World},
};

use super::BedrockClient;

const REGIONAL_PACKET_BUDGET: usize = 8;
const REGIONAL_MOVEMENT_BYTES: usize = 512;

// The raw packet remains available for translation, hook edits and changes of owner.
pub(super) struct QueuedPlayInput {
    packet: RawPacket,
    movement: Option<PreparedMovement>,
}

struct PreparedMovement {
    extent: Weak<[Vector2<i32>]>,
    input: SPlayerAuthInput,
}

impl PreparedMovement {
    fn owned_extent(&self, player: &Player) -> Option<RegionChunks> {
        let extent = self.extent.upgrade()?;
        let world = player.world();
        let layout = world.region_layout.load();
        (player.can_simulate_in(&world)
            && layout
                .owner(player.get_entity().chunk_pos.load())
                .and_then(|owner| layout.chunks(owner))
                .is_some_and(|current| Arc::ptr_eq(current, &extent)))
        .then_some(extent)
    }
}

impl BedrockClient {
    pub(super) fn enqueue_play_input(&self, player: &Arc<Player>, packet: RawPacket) {
        let ready = (Self::is_regional_reply(packet.id) || Self::can_prepare_movement(&packet))
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
            let Some(QueuedPlayInput { packet, movement }) = head.take().or_else(|| {
                player.inbound_packets.pop().map(|packet| QueuedPlayInput {
                    packet,
                    movement: None,
                })
            }) else {
                break;
            };
            if self.is_closed() {
                break;
            }
            let original = movement.as_ref().map(|_| packet.payload.clone());
            let Some(packet) = self.filter_play_packet(&server, &player, packet) else {
                continue;
            };
            if let Some((movement, original)) = movement.zip(original)
                && packet.id == SPlayerAuthInput::PACKET_ID
                && std::ptr::eq(packet.payload.as_ref(), original.as_ref())
                && self.is_current_protocol()
                && let Some(extent) = movement.owned_extent(&player)
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
        })
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
        if head.is_some() {
            return false;
        }
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
            let Some(packet) = player.inbound_packets.pop() else {
                return false;
            };
            // Keep the first world-owned packet ahead of everything behind it.
            if !Self::is_regional_reply(packet.id) {
                let payload = Self::can_prepare_movement(&packet).then(|| packet.payload.clone());
                *head = Some(QueuedPlayInput {
                    packet,
                    movement: None,
                });
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
    use pumpkin_config::{
        AdvancedConfiguration, BasicConfiguration, telemetry::TelemetryConfig, world::WorldPreset,
    };
    use pumpkin_data::Block;
    use pumpkin_protocol::serial::PacketWrite;
    use pumpkin_util::{GameMode, math::vector3::Vector3};
    use pumpkin_world::world::BlockFlags;
    use std::{
        sync::atomic::Ordering::Relaxed,
        time::{Duration, Instant},
    };

    use crate::{
        data::VanillaData,
        net::{ClientPlatform, GameProfile, PlayerConfig, packet_limiter::PacketRateLimiter},
        server::TickDeadline,
    };

    const TICK_PERIOD: Duration = Duration::from_millis(50);

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
        tick_region(world, origin, next);
        tick_region(world, destination, first);
        assert_eq!(delay(), 1);
        tick_region(world, destination, next);
        assert_eq!(delay(), 0);
        assert!(player.begin_experience_pickup());
    }

    async fn verify_mining_clock(
        world: &Arc<World>,
        server: &Server,
        player: &Arc<Player>,
        client: &BedrockClient,
        chunk: Vector2<i32>,
    ) {
        world.level.get_or_fetch_chunk(chunk, |_| ()).await;
        let target = BlockPos::new(9, 100, 8);
        let dirt = Block::DIRT.default_state.id;
        let stone = Block::STONE.default_state.id;
        world.set_block_state(&target, dirt, BlockFlags::FORCE_STATE);
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
        tick_region(world, chunk, at + TICK_PERIOD);
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

        world.set_block_state(&target, stone, BlockFlags::FORCE_STATE);
        tick_region(world, chunk, at + TICK_PERIOD * 2);
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
        advance(west);
        assert_eq!(player.inbound_packets.len(), 1);
        let pending_with_lock = {
            let _draining = client.activity.inbound_head.lock().unwrap();
            advance(east);
            player.inbound_packets.len()
        };
        assert_eq!(pending_with_lock, 1);
        advance(east);
        check();
        verify_pickup_timer_handoff(&world, &player, east, west);
        verify_mining_clock(&world, &server, &player, &client, west).await;

        world.players.store(Arc::default());
        client.close().await;
        world.level.shutdown().await;
    }
}
