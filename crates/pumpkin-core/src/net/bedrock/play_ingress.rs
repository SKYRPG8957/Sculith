use std::sync::{Arc, MutexGuard};

use pumpkin_protocol::{
    RawPacket,
    bedrock::{
        network_stack_latency::NetworkStackLatency,
        server::client_cache_blob_status::SClientCacheBlobStatus,
    },
    packet::Packet,
    serial::PacketRead,
};

use crate::{
    entity::player::Player,
    plugin::server::packet::{PacketReceivedEvent, PacketSentEvent},
    server::Server,
};

use super::BedrockClient;

const REGIONAL_PACKET_BUDGET: usize = 8;

impl BedrockClient {
    pub(super) fn enqueue_play_input(&self, player: &Arc<Player>, packet: RawPacket) {
        let ready = Self::is_regional_input(packet.id) && self.is_current_protocol();
        player.inbound_packets.push(packet);
        if ready {
            player.world().offer_regional_input(self.id, player);
        }
    }

    // The guard serializes regional input and the ordinary play drain for one connection.
    pub(crate) fn try_inbound_head(&self) -> Option<MutexGuard<'_, Option<RawPacket>>> {
        self.activity.inbound_head.try_lock().ok()
    }

    const fn is_regional_input(id: i32) -> bool {
        matches!(
            id,
            NetworkStackLatency::PACKET_ID | SClientCacheBlobStatus::PACKET_ID
        )
    }

    pub(crate) fn drain_regional_input(
        &self,
        player: &Player,
        server: &Server,
        owns_player: impl Fn() -> bool,
    ) -> bool {
        let Some(mut head) = self.try_inbound_head() else {
            return false;
        };
        for _ in 0..REGIONAL_PACKET_BUDGET {
            if !owns_player()
                || !self.is_current_protocol()
                || server.plugin_manager.has_handlers::<PacketReceivedEvent>()
                || server.plugin_manager.has_handlers::<PacketSentEvent>()
            {
                return false;
            }
            let Some(packet) = head.take().or_else(|| player.inbound_packets.pop()) else {
                return false;
            };
            // Keep the first world-owned packet ahead of everything behind it.
            if !Self::is_regional_input(packet.id) {
                *head = Some(packet);
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
