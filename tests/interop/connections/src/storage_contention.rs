use std::{
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use pumpkin_core::{
    entity::player::Player,
    plugin::{
        BoxFuture, Context, EventHandler, EventPriority,
        server::{packet::PacketReceivedEvent, server_tick_start::ServerTickStartEvent},
    },
    server::Server,
};
use pumpkin_data::item_stack::ItemStack;
use pumpkin_inventory::{Inventory, slot::Slot};
use pumpkin_protocol::{RawPacket, java::server::play::SClickSlot, packet::MultiVersionJavaPacket};

pub fn register(context: &Arc<Context>) {
    let probe = Arc::new(Probe {
        context: Arc::downgrade(context),
        round: Mutex::default(),
    });
    context.register_event::<PacketReceivedEvent, _>(probe.clone(), EventPriority::Lowest, true);
    context.register_event::<ServerTickStartEvent, _>(probe, EventPriority::Lowest, true);
}

struct Probe {
    context: Weak<Context>,
    round: Mutex<Round>,
}

#[derive(Default)]
struct Round {
    java: Option<(Arc<Player>, RawPacket)>,
    bedrock: Option<(Arc<Player>, RawPacket)>,
    replayed: bool,
}

impl EventHandler<PacketReceivedEvent> for Probe {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut PacketReceivedEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let java = match event.player.gameprofile.name.as_str() {
                "RaceChestJava" => {
                    let Some(client) = event.player.client.java() else {
                        return;
                    };
                    if event.packet_id != SClickSlot::to_id(client.version.load()) {
                        return;
                    }
                    true
                }
                "RaceChestBed"
                    if event.packet_id == pumpkin_data::bedrock_packet::ITEM_STACK_REQUEST =>
                {
                    false
                }
                _ => return,
            };
            let mut round = self
                .round
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if round.replayed {
                return;
            }
            let pending = if java {
                &mut round.java
            } else {
                &mut round.bedrock
            };
            *pending = Some((
                event.player.clone(),
                RawPacket {
                    id: event.packet_id,
                    payload: event.payload.clone(),
                },
            ));
            event.cancelled = true;
        })
    }
}

impl EventHandler<ServerTickStartEvent> for Probe {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut ServerTickStartEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let requests = {
                let mut round = self
                    .round
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if round.java.is_none() || round.bedrock.is_none() {
                    return;
                }
                round.replayed = true;
                [round.java.take(), round.bedrock.take()]
            };
            let gate = Arc::new(ReadGate::default());
            for ((player, packet), wait_at) in requests.into_iter().flatten().zip([3, 2]) {
                let screen = player
                    .current_screen_handler
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                let mut screen = screen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(slot) = screen.get_behaviour_mut().slots.first_mut() {
                    *slot = Arc::new(ContendedSlot {
                        inner: slot.clone(),
                        gate: gate.clone(),
                        reads: AtomicUsize::new(0),
                        wait_at,
                    });
                }
                player.inbound_packets.push(packet);
            }
            if let Some(ctx) = self.context.upgrade() {
                ctx.log(format!(
                    "PROBE_STORAGE_CONTENTION tick={} queued=2",
                    event.tick
                ));
            }
        })
    }
}

#[derive(Default)]
struct ReadGate {
    readers: Mutex<u8>,
    arrived: Condvar,
}

struct ContendedSlot {
    inner: Arc<dyn Slot>,
    gate: Arc<ReadGate>,
    reads: AtomicUsize,
    wait_at: usize,
}

impl Slot for ContendedSlot {
    fn get_stack(&self) -> ItemStack {
        let stack = self.inner.get_stack();
        // Yield after reading the value used for the Java quick move or Bedrock commit.
        // A serial dispatcher cannot reach the other read until this request finishes.
        if self.reads.fetch_add(1, Ordering::Relaxed) + 1 == self.wait_at {
            let mut readers = self
                .gate
                .readers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *readers += 1;
            self.gate.arrived.notify_all();
            drop(self.gate.arrived.wait_timeout_while(
                readers,
                Duration::from_millis(300),
                |count| *count < 2,
            ));
        }
        stack
    }

    fn get_inventory(&self) -> Arc<dyn Inventory> {
        self.inner.get_inventory()
    }

    fn get_index(&self) -> usize {
        self.inner.get_index()
    }

    fn set_id(&self, index: usize) {
        self.inner.set_id(index);
    }

    fn mark_dirty(&self) {
        self.inner.mark_dirty();
    }
}
