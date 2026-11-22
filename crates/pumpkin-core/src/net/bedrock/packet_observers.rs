use std::sync::{Arc, Weak};

use pumpkin_protocol::RawPacket;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use crate::entity::player::Player;
use crate::plugin::server::packet::PacketReceivedEvent;
use crate::plugin::{HandlerMap, Payload, PluginManager};
use crate::server::Server;
use crate::world::World;

use super::BedrockClient;

const MAX_PENDING_PACKETS: usize = 32;
const MAX_PENDING_PAYLOAD_BYTES: usize = 1024 * 1024;

pub(super) struct PacketObservers {
    pending: mpsc::Sender<Observation>,
    bytes: Arc<Semaphore>,
}

struct Observation {
    packet: RawPacket,
    cancelled: bool,
    world: Weak<World>,
    handlers: Weak<HandlerMap>,
    _bytes: OwnedSemaphorePermit,
}

impl PacketObservers {
    fn start(client: &Arc<BedrockClient>, player: &Arc<Player>) -> Self {
        let (pending, receiver) = mpsc::channel(MAX_PENDING_PACKETS);
        let connection = client.clone();
        let session = Arc::downgrade(player);
        let server = client.server.clone();
        let _ = client.spawn_task(async move {
            tokio::select! {
                biased;
                () = connection.await_close_interrupt() => {},
                () = Self::deliver(receiver, session, server) => {},
            }
        });
        Self {
            pending,
            bytes: Arc::new(Semaphore::new(MAX_PENDING_PAYLOAD_BYTES)),
        }
    }

    fn enqueue(&self, event: &PacketReceivedEvent, handlers: &Arc<HandlerMap>) {
        let Ok(position) = self.pending.try_reserve() else {
            return;
        };
        let Ok(length) = u32::try_from(event.payload.len().max(1)) else {
            return;
        };
        let Ok(bytes) = self.bytes.clone().try_acquire_many_owned(length) else {
            return;
        };
        position.send(Observation {
            packet: RawPacket {
                id: event.packet_id,
                payload: event.payload.clone(),
            },
            cancelled: event.cancelled,
            world: Arc::downgrade(&event.player.world()),
            handlers: Arc::downgrade(handlers),
            _bytes: bytes,
        });
    }

    async fn deliver(
        mut pending: mpsc::Receiver<Observation>,
        session: Weak<Player>,
        server: Weak<Server>,
    ) {
        while let Some(observation) = pending.recv().await {
            let (Some(player), Some(server)) = (session.upgrade(), server.upgrade()) else {
                return;
            };
            let (Some(world), Some(handlers)) =
                (observation.world.upgrade(), observation.handlers.upgrade())
            else {
                continue;
            };
            // A queued notification cannot become an invocation of a reloaded plugin.
            if !player.can_tick_in(&world)
                || !Arc::ptr_eq(&handlers, &server.plugin_manager.handlers.load())
            {
                continue;
            }
            let Some(observers) = handlers.get(PacketReceivedEvent::get_name_static()) else {
                continue;
            };
            let event = PacketReceivedEvent {
                player,
                packet_id: observation.packet.id,
                payload: observation.packet.payload,
                cancelled: observation.cancelled,
            };
            PluginManager::dispatch_observers(&server, observers, &event).await;
        }
    }
}

impl BedrockClient {
    pub(crate) fn filter_play_packet(
        self: &Arc<Self>,
        server: &Arc<Server>,
        player: &Arc<Player>,
        packet: RawPacket,
    ) -> Option<RawPacket> {
        let snapshot = server.plugin_manager.handlers.load();
        let Some(handlers) = snapshot.get(PacketReceivedEvent::get_name_static()) else {
            return Some(packet);
        };
        if handlers.is_empty() {
            return Some(packet);
        }
        let mut event = PacketReceivedEvent::new(player.clone(), packet.id, packet.payload);
        PluginManager::apply_packet_filters(server, handlers, &mut event);
        if handlers.iter().any(|handler| !handler.is_blocking()) {
            self.activity
                .packet_observers
                .get_or_init(|| PacketObservers::start(self, player))
                .enqueue(&event, &snapshot);
        }
        (!event.cancelled).then_some(RawPacket {
            id: event.packet_id,
            payload: event.payload,
        })
    }
}
