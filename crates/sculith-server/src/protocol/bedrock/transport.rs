//! Bounded Tokio bridge for the sans-IO RakNet state machines.
//! Owns the already bound socket, including port 0, and every session's lifetime.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Result, ensure};
use bedrock::{
    network::tokio::{Connection, error::TransportLayerError, transport::Transport},
    protocol::{ProtoVersion, V2225},
};
use raknet::prelude::*;
use sculith_core::ServerStatus;
use tokio::{
    net::UdpSocket,
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinSet,
    time::MissedTickBehavior,
};

use super::session::{Authentication, handle};
use crate::world::WorldHandle;

pub const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_SESSIONS: usize = 64;
const INGRESS_CAPACITY: usize = 16;
const QUEUED_BYTES: usize = 16 * 1024 * 1024;

struct Inbound {
    data: Vec<u8>,
    _bytes: OwnedSemaphorePermit,
}

struct SendCommand {
    id: RakSessionId,
    payload: Box<[u8]>,
    result: oneshot::Sender<Result<(), String>>,
    _bytes: OwnedSemaphorePermit,
}

pub struct SessionTransport {
    id: RakSessionId,
    address: SocketAddr,
    input: mpsc::Receiver<Inbound>,
    commands: mpsc::Sender<SendCommand>,
    closed: Arc<AtomicBool>,
    output_budget: Arc<Semaphore>,
}

impl Drop for SessionTransport {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
    }
}

fn io_error(message: impl Into<String>) -> TransportLayerError {
    std::io::Error::other(message.into()).into()
}

impl Transport for SessionTransport {
    async fn send(&mut self, batch: &[u8]) -> Result<(), TransportLayerError> {
        if batch.len() + 1 > MAX_MESSAGE_BYTES || self.closed.load(Ordering::Acquire) {
            return Err(io_error("session closed or outgoing message exceeds 4 MiB"));
        }
        let reservation = self
            .output_budget
            .clone()
            .acquire_many_owned((batch.len() + 1) as u32)
            .await
            .map_err(|_| io_error("outgoing byte budget closed"))?;
        let mut payload = Vec::with_capacity(batch.len() + 1);
        payload.push(0xfe);
        payload.extend_from_slice(batch);
        let (sender, receiver) = oneshot::channel();
        self.commands
            .send(SendCommand {
                id: self.id,
                payload: payload.into_boxed_slice(),
                result: sender,
                _bytes: reservation,
            })
            .await
            .map_err(|_| io_error("RakNet driver stopped"))?;
        receiver
            .await
            .map_err(|_| io_error("RakNet driver dropped send"))?
            .map_err(io_error)
    }

    async fn recv(&mut self) -> Result<Vec<u8>, TransportLayerError> {
        let packet = self
            .input
            .recv()
            .await
            .ok_or_else(|| io_error("RakNet session closed"))?;
        if packet.data.first() != Some(&0xfe) {
            return Err(io_error("invalid Bedrock game packet marker"));
        }
        Ok(packet.data[1..].to_vec())
    }

    fn remote_addr(&self) -> SocketAddr {
        self.address
    }
    fn max_message_len(&self) -> usize {
        MAX_MESSAGE_BYTES
    }
    async fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
    async fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

struct Entry {
    session: RakSession,
    input: mpsc::Sender<Inbound>,
    closed: Arc<AtomicBool>,
}

pub async fn serve(
    socket: UdpSocket,
    mut status: watch::Receiver<ServerStatus>,
    guid: u64,
    authentication: Authentication,
    timeout: Duration,
    mut shutdown: watch::Receiver<bool>,
    world: WorldHandle,
) -> Result<()> {
    let address = socket.local_addr()?;
    let capacity = (status.borrow().max_players as usize).min(MAX_SESSIONS);
    let config = RakServerConfig {
        guid,
        protocols: Box::new([V2225::RAKNET_VERSION]),
        max_connections: capacity,
        max_connections_per_ip: Some(4),
        ..RakServerConfig::default()
    };
    let mut server = RakServer::new(config, address);
    let mut sessions: HashMap<RakSessionId, Entry> = HashMap::new();
    let (commands_tx, mut commands_rx) = mpsc::channel::<SendCommand>(64);
    let input_budget = Arc::new(Semaphore::new(QUEUED_BYTES));
    let output_budget = Arc::new(Semaphore::new(QUEUED_BYTES));
    let mut tasks = JoinSet::new();
    let mut buffer = [0; 2048];
    let reference = super::super::references().bedrock;
    let (port4, port6) = if address.is_ipv4() {
        (address.port(), 0)
    } else {
        (0, address.port())
    };
    let mut limiter = super::RateLimiter::new(Instant::now());
    let mut update = tokio::time::interval(Duration::from_millis(10));
    update.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let result = loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break Ok(()); }
            }
            received = socket.recv_from(&mut buffer) => {
                let (length, remote) = match received { Ok(value) => value, Err(error) => break Err(error.into()) };
                let packet = &buffer[..length];
                if matches!(packet.first(), Some(0x01 | 0x02)) {
                    if super::valid_ping(packet) && limiter.allow(remote.ip(), Instant::now()) {
                        let snapshot = status.borrow().clone();
                        let pong = super::make_pong(packet, &snapshot, guid, &reference, port4, port6);
                        if let Err(error) = socket.send_to(&pong, remote).await {
                            tracing::debug!(%remote, %error, "Bedrock discovery reply failed");
                        }
                    }
                } else if let Err(error) = server.handle(RakServerInput::Datagram(packet.into(), remote, SystemTime::now())) {
                    tracing::debug!(%remote, %error, "invalid RakNet datagram");
                }
            }
            _ = update.tick() => {
                let now = SystemTime::now();
                let _ = server.handle(RakServerInput::Update(now));
                for entry in sessions.values_mut() {
                    let input = if (entry.closed.load(Ordering::Acquire) || entry.input.is_closed())
                        && entry.session.state != RakSessionState::Closing {
                        RakSessionInput::Disconnect(now)
                    } else { RakSessionInput::Update(now) };
                    if entry.session.handle(input).is_err() { entry.closed.store(true, Ordering::Release); }
                }
            }
            Some(command) = commands_rx.recv() => {
                let result = match sessions.get_mut(&command.id) {
                    Some(entry) if !entry.closed.load(Ordering::Acquire) => entry.session.handle(RakSessionInput::Send(
                        command.payload, RakReliability::ReliableOrdered, RakPriority::Immediate, SystemTime::now()
                    )).map_err(|error| error.to_string()),
                    _ => Err("session no longer exists".into()),
                };
                let _ = command.result.send(result);
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(error) = result { tracing::warn!(%error, "Bedrock connection task failed"); }
            }
            changed = status.changed() => {
                if changed.is_err() { break Ok(()); }
            }
        }
        while let Some(event) = server.poll() {
            match event {
                RakServerOutput::SocketDatagram(packet, remote) => {
                    if let Err(error) = socket.send_to(&packet, remote).await {
                        tracing::debug!(%remote, %error, "RakNet reply failed");
                    }
                }
                RakServerOutput::SessionDatagram(packet, id) => {
                    if let Some(entry) = sessions.get_mut(&id)
                        && entry
                            .session
                            .handle(RakSessionInput::Datagram(packet, SystemTime::now()))
                            .is_err()
                    {
                        entry.closed.store(true, Ordering::Release);
                    }
                }
                RakServerOutput::SessionConnected(session) => {
                    if sessions.len() >= capacity {
                        let _ = server
                            .handle(RakServerInput::RemoveSession(session.id, SystemTime::now()));
                        continue;
                    }
                    // Preserve handshake/sequence state using the upstream
                    // snapshot API while lowering its default 64 MiB budgets.
                    let mut snapshot = session.snapshot();
                    snapshot.config.max_queued_bytes = MAX_MESSAGE_BYTES as i32;
                    snapshot.config.max_outbound_bytes = MAX_MESSAGE_BYTES as i32;
                    let session = match RakSession::restore(snapshot) {
                        Ok(session) => session,
                        Err(error) => {
                            tracing::warn!(%error, "cannot initialize bounded RakNet session");
                            let _ = server.handle(RakServerInput::RemoveSession(
                                session.id,
                                SystemTime::now(),
                            ));
                            continue;
                        }
                    };
                    let (input_tx, input_rx) = mpsc::channel(INGRESS_CAPACITY);
                    let closed = Arc::new(AtomicBool::new(false));
                    let transport = SessionTransport {
                        id: session.id,
                        address: session.addr,
                        input: input_rx,
                        commands: commands_tx.clone(),
                        closed: closed.clone(),
                        output_budget: output_budget.clone(),
                    };
                    sessions.insert(
                        session.id,
                        Entry {
                            session,
                            input: input_tx,
                            closed,
                        },
                    );
                    let authentication = authentication.clone();
                    let world = world.clone();
                    tasks.spawn(async move {
                        let remote = transport.address;
                        let mut conn = Connection::<V2225, _>::from_transport_conn(transport);
                        conn.max_batch_len = MAX_MESSAGE_BYTES;
                        let result = handle(&mut conn, &authentication, &world, timeout).await;
                        match result {
                            Ok(()) => {}
                            Err(error) => {
                                tracing::debug!(%remote, error = ?error, "Bedrock session ended")
                            }
                        }
                        conn.close().await;
                    });
                }
            }
        }
        let mut removed = Vec::new();
        for (&id, entry) in &mut sessions {
            while let Some(event) = entry.session.poll() {
                match event {
                    RakSessionOutput::Packet(packet) => {
                        if packet.len() > MAX_MESSAGE_BYTES {
                            entry.closed.store(true, Ordering::Release);
                        } else if let Ok(reservation) = input_budget
                            .clone()
                            .try_acquire_many_owned(packet.len() as u32)
                        {
                            if entry
                                .input
                                .try_send(Inbound {
                                    data: packet.into_vec(),
                                    _bytes: reservation,
                                })
                                .is_err()
                            {
                                entry.closed.store(true, Ordering::Release);
                            }
                        } else {
                            tracing::debug!(session = ?id, "closing session: incoming byte budget exhausted");
                            entry.closed.store(true, Ordering::Release);
                        }
                    }
                    RakSessionOutput::Datagram(packet, remote) => {
                        if let Err(error) = socket.send_to(&packet, remote).await {
                            tracing::debug!(%remote, %error, "RakNet session send failed");
                        }
                    }
                    RakSessionOutput::Disconnected(_, _) => {
                        removed.push(id);
                    }
                    RakSessionOutput::Wait(_) => {}
                }
            }
        }
        for id in removed {
            if let Some(entry) = sessions.remove(&id) {
                entry.closed.store(true, Ordering::Release);
            }
            let _ = server.handle(RakServerInput::RemoveSession(id, SystemTime::now()));
        }
    };
    for entry in sessions.values() {
        entry.closed.store(true, Ordering::Release);
    }
    sessions.clear();
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    ensure!(tasks.is_empty(), "Bedrock tasks did not stop");
    result
}
