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
            return false;
        };
        if head.is_some() {
            return false;
        }
        for _ in 0..REGIONAL_PACKET_BUDGET {
            if !owns(player.get_entity().chunk_pos.load())
                || !self.is_current_protocol()
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
