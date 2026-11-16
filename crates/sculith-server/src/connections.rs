//! Bounded, opt-in connection observations, independent of world reservations.
use sculith_core::connections::{
    ConnectionEvent, ConnectionPacket, ConnectionTransport, PacketPhase,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::broadcast;

const CAPACITY: usize = 128;

struct Shared {
    enabled: AtomicBool,
    sent: AtomicBool,
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
            sent: AtomicBool::new(false),
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

    pub fn configure_sent(&self, enabled: bool) {
        self.0.sent.store(enabled, Ordering::Release);
    }

    pub fn open(&self, transport: ConnectionTransport) -> Trace {
        let enabled = self.0.enabled.load(Ordering::Acquire) || self.0.sent.load(Ordering::Acquire);
        let id = enabled
            .then(|| {
                self.0
                    .next_id
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                    .ok()
            })
            .flatten();
        let trace = Trace(id.map(|id| {
            Arc::new(Active {
                observer: self.clone(),
                id,
                state: Mutex::new(State {
                    received: 0,
                    sent: 0,
                    attached: false,
                    protocol: None,
                    phase: PacketPhase::Handshake,
                }),
            })
        }));
        if let Some(active) = &trace.0 {
            active.send(ConnectionEvent::Opened {
                connection_id: active.id,
                transport,
            });
        }
        trace
    }
}

struct State {
    received: u64,
    sent: u64,
    attached: bool,
    protocol: Option<u32>,
    phase: PacketPhase,
}
struct Active {
    observer: Observer,
    id: u64,
    state: Mutex<State>,
}
impl Active {
    fn send(&self, event: ConnectionEvent) {
        if self.observer.0.enabled.load(Ordering::Acquire)
            || self.observer.0.sent.load(Ordering::Acquire)
        {
            let _ = self.observer.0.events.send(event);
        }
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        self.send(ConnectionEvent::Closed {
            connection_id: self.id,
        });
    }
}

/// Clones share one lifecycle. Disabled traces have no allocation or ID.
#[derive(Default, Clone)]
pub struct Trace(Option<Arc<Active>>);

impl Trace {
    pub fn enabled(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|a| a.observer.0.enabled.load(Ordering::Acquire))
    }
    pub fn sent_enabled(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|a| a.observer.0.sent.load(Ordering::Acquire))
    }
    pub fn select_protocol(&self, protocol: u32) {
        if let Some(a) = &self.0 {
            a.state.lock().unwrap().protocol = Some(protocol);
        }
    }
    pub fn set_phase(&self, phase: PacketPhase) {
        if let Some(a) = &self.0 {
            a.state.lock().unwrap().phase = phase;
        }
    }
    pub fn context(&self) -> Option<(Option<u32>, PacketPhase)> {
        let a = self.0.as_ref()?;
        let state = a.state.lock().unwrap();
        Some((state.protocol, state.phase))
    }
    pub fn packet(&self, packet: ConnectionPacket) {
        self.record(packet, false);
    }
    pub fn sent(&self, packet: ConnectionPacket) {
        self.record(packet, true);
    }
    fn record(&self, mut packet: ConnectionPacket, sent: bool) {
        if !(if sent {
            self.sent_enabled()
        } else {
            self.enabled()
        }) {
            return;
        }
        let Some(a) = &self.0 else {
            return;
        };
        let mut state = a.state.lock().unwrap();
        let counter = if sent {
            &mut state.sent
        } else {
            &mut state.received
        };
        let Some(sequence) = counter.checked_add(1) else {
            return;
        };
        packet.sequence = sequence;
        debug_assert!(packet.validate().is_ok());
        *counter = sequence;
        a.send(if sent {
            ConnectionEvent::PacketSent {
                connection_id: a.id,
                packet,
            }
        } else {
            ConnectionEvent::PacketReceived {
                connection_id: a.id,
                packet,
            }
        });
    }
    pub fn attach(&self, player_id: u64) {
        let Some(a) = &self.0 else {
            return;
        };
        let mut state = a.state.lock().unwrap();
        if !state.attached && player_id != 0 {
            state.attached = true;
            a.send(ConnectionEvent::PlayerAttached {
                connection_id: a.id,
                player_id,
            });
        }
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
        let trace = observer.open(ConnectionTransport::JavaTcp);
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
        let a = observer.open(ConnectionTransport::JavaTcp);
        let b = observer.open(ConnectionTransport::BedrockNethernet);
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

    #[test]
    fn cloned_directions_keep_independent_sequences_and_close_after_last_owner() {
        let observer = Observer::default();
        let mut events = observer.subscribe();
        observer.configure(true);
        observer.configure_sent(true);
        let receiver = observer.open(ConnectionTransport::JavaTcp);
        let sender = receiver.clone();
        receiver.packet(packet());
        sender.sent(packet());
        sender.sent(packet());
        receiver.packet(packet());
        drop(receiver);
        let mut received = Vec::new();
        let mut sent = Vec::new();
        while let Ok(event) = events.try_recv() {
            match event {
                ConnectionEvent::PacketReceived { packet, .. } => received.push(packet.sequence),
                ConnectionEvent::PacketSent { packet, .. } => sent.push(packet.sequence),
                ConnectionEvent::Closed { .. } => panic!("sender still owns lifecycle"),
                _ => {}
            }
        }
        assert_eq!(received, [1, 2]);
        assert_eq!(sent, [1, 2]);
        observer.configure_sent(false);
        sender.sent(packet());
        assert!(events.try_recv().is_err());
        drop(sender);
        assert!(matches!(
            events.try_recv().unwrap(),
            ConnectionEvent::Closed { connection_id: 1 }
        ));
        assert!(events.try_recv().is_err());
    }

    #[test]
    #[ignore = "local release-mode observer microbenchmark"]
    fn connection_observer_benchmark() {
        use std::hint::black_box;
        for (receive, send) in [(false, false), (true, false), (true, true)] {
            let observer = Observer::default();
            let mut events = observer.subscribe();
            observer.configure(receive);
            observer.configure_sent(send);
            let mut samples = Vec::new();
            for _ in 0..5 {
                let started = std::time::Instant::now();
                for _ in 0..10_000 {
                    let trace = observer.open(ConnectionTransport::JavaTcp);
                    let sender = trace.clone();
                    if trace.enabled() {
                        trace.packet(packet());
                    }
                    if sender.sent_enabled() {
                        sender.sent(packet());
                    }
                    trace.attach(7);
                    drop(trace);
                    drop(sender);
                    while let Ok(event) = events.try_recv() {
                        black_box(event);
                    }
                }
                samples.push(started.elapsed());
            }
            samples.sort();
            println!(
                "OBSERVER receive={receive} send={send} cycles=10000 median={:?}",
                samples[2]
            );
        }
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
        let trace = observer.open(ConnectionTransport::JavaTcp);
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
