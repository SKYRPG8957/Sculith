//! Bounded, opt-in connection observations, independent of world reservations.
use sculith_core::connections::{ConnectionEvent, ConnectionPacket, ConnectionTransport};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::broadcast;

const CAPACITY: usize = 128;

struct Shared {
    enabled: AtomicBool,
    next_id: AtomicU64,
    events: broadcast::Sender<ConnectionEvent>,
}

#[derive(Clone)]
pub struct Observer(Arc<Shared>);

impl Default for Observer {
    fn default() -> Self {
        let (events, _) = broadcast::channel(CAPACITY);
        Self(Arc::new(Shared {
            enabled: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            events,
        }))
    }
}

impl Observer {
    pub fn subscribe(&self) -> broadcast::Receiver<ConnectionEvent> {
        self.0.events.subscribe()
    }

    pub fn configure(&self, enabled: bool) {
        self.0.enabled.store(enabled, Ordering::Release);
    }

    pub fn open(&self, transport: ConnectionTransport) -> Trace {
        let id = self
            .0
            .enabled
            .load(Ordering::Acquire)
            .then(|| {
                self.0
                    .next_id
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                    .ok()
            })
            .flatten();
        let trace = Trace {
            observer: id.map(|_| self.clone()),
            id: id.unwrap_or(0),
            sequence: 0,
            attached: false,
        };
        trace.send(ConnectionEvent::Opened {
            connection_id: trace.id,
            transport,
        });
        trace
    }
}

#[derive(Default)]
pub struct Trace {
    observer: Option<Observer>,
    id: u64,
    sequence: u64,
    attached: bool,
}

impl Trace {
    pub fn enabled(&self) -> bool {
        self.observer
            .as_ref()
            .is_some_and(|o| o.0.enabled.load(Ordering::Acquire))
    }

    fn send(&self, event: ConnectionEvent) {
        if let Some(observer) = &self.observer
            && observer.0.enabled.load(Ordering::Acquire)
        {
            let _ = observer.0.events.send(event);
        }
    }

    pub fn packet(&mut self, mut packet: ConnectionPacket) {
        if !self.enabled() {
            return;
        }
        let Some(sequence) = self.sequence.checked_add(1) else {
            return;
        };
        packet.sequence = sequence;
        debug_assert!(packet.validate().is_ok());
        self.sequence = sequence;
        self.send(ConnectionEvent::PacketReceived {
            connection_id: self.id,
            packet,
        });
    }

    pub fn attach(&mut self, player_id: u64) {
        if !self.attached && player_id != 0 {
            self.attached = true;
            self.send(ConnectionEvent::PlayerAttached {
                connection_id: self.id,
                player_id,
            });
        }
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        self.send(ConnectionEvent::Closed {
            connection_id: self.id,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sculith_core::{PacketDispatch, connections::PacketPhase};

    fn packet() -> ConnectionPacket {
        ConnectionPacket {
            protocol: Some(777),
            phase: PacketPhase::Login,
            id: 0,
            name: "minecraft:hello".into(),
            dispatch: PacketDispatch::Control,
            sequence: 0,
        }
    }

    #[test]
    fn disabled_observations_do_not_allocate_ids_or_emit_events() {
        let observer = Observer::default();
        let mut events = observer.subscribe();
        let mut trace = observer.open(ConnectionTransport::JavaTcp);
        assert!(!trace.enabled());
        trace.packet(packet());
        trace.attach(7);
        drop(trace);
        assert!(events.try_recv().is_err());
        assert_eq!(observer.0.next_id.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn connection_ids_and_sequences_are_independent_and_close_once() {
        let observer = Observer::default();
        let mut events = observer.subscribe();
        observer.configure(true);
        let mut a = observer.open(ConnectionTransport::JavaTcp);
        let mut b = observer.open(ConnectionTransport::BedrockNethernet);
        a.packet(packet());
        b.packet(packet());
        a.packet(packet());
        a.attach(42);
        a.attach(42);
        drop(a);
        drop(b);
        let mut packets = Vec::new();
        let mut attached = 0;
        let mut closed = Vec::new();
        while let Ok(event) = events.try_recv() {
            match event {
                ConnectionEvent::PacketReceived {
                    connection_id,
                    packet,
                } => {
                    packet.validate().unwrap();
                    packets.push((connection_id, packet.sequence));
                }
                ConnectionEvent::PlayerAttached {
                    connection_id,
                    player_id,
                } => {
                    assert_eq!((connection_id, player_id), (1, 42));
                    attached += 1;
                }
                ConnectionEvent::Closed { connection_id } => closed.push(connection_id),
                _ => {}
            }
        }
        assert_eq!(packets, [(1, 1), (2, 1), (1, 2)]);
        assert_eq!(attached, 1);
        assert_eq!(closed, [1, 2]);
    }

    #[tokio::test]
    async fn cancelled_connection_closes_and_ring_overflow_is_explicit() {
        let observer = Observer::default();
        let mut events = observer.subscribe();
        observer.configure(true);
        let copy = observer.clone();
        let task = tokio::spawn(async move {
            let _trace = copy.open(ConnectionTransport::JavaTcp);
            std::future::pending::<()>().await;
        });
        assert!(matches!(
            events.recv().await.unwrap(),
            ConnectionEvent::Opened {
                connection_id: 1,
                ..
            }
        ));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(matches!(
            events.recv().await.unwrap(),
            ConnectionEvent::Closed { connection_id: 1 }
        ));
        let mut trace = observer.open(ConnectionTransport::JavaTcp);
        for _ in 0..=CAPACITY {
            trace.packet(packet());
        }
        assert!(matches!(
            events.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
        observer.configure(false);
        assert!(!trace.enabled());
    }
}
