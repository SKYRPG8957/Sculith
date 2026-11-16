//! Bounded loopback HTTP signaling and a sans-IO WebRTC driver.
//! Wire facts: Mojang's NetherNetOnboardingGuide; Apache-2.0 nethernet
//! ed552b0b60dc6f8d54e0d7d42a9c66a9b9bdfb99. Gameplay uses the existing owner.

use super::{
    session::{Protection, Settings, handle_with_protection},
    transport::MAX_MESSAGE_BYTES,
};
use crate::world::WorldHandle;
use anyhow::{Context, Result, ensure};
use bedrock::{
    network::tokio::{Connection, error::TransportLayerError, transport::Transport},
    protocol::{ProtoVersion, V2225},
};
use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use nethernet::{
    connection::{Connection as Peer, ConnectionInput, IceMode},
    prelude::{
        HttpSignaler, HttpSignalerConfig, HttpSignalerInput, HttpSignalerOutput, Offer,
        RejectReason, Sans, ServerData, ServerIdentity, TokenTrust,
    },
    session::{Channel, Session, SessionEvent, SessionOutput},
};
use sculith_core::ServerStatus;
use std::{
    collections::HashMap,
    convert::Infallible,
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinSet,
    time::MissedTickBehavior,
};

const HTTP_CONNECTIONS: usize = 16;
const HTTP_BODY_BYTES: usize = 64 * 1024;
const MEDIA_CONNECTIONS: usize = 64;
const PER_IP: usize = 4;
const QUEUED_BYTES: usize = 16 * 1024 * 1024;
const NETHER_MESSAGE_BYTES: usize = 255 * 10_000;
const IDENTITY_BYTES: u64 = 64 * 1024;

mod media;
use media::LocalMedia;

pub struct Server {
    listener: TcpListener,
    identity: Arc<ServerIdentity>,
    media: Arc<LocalMedia>,
}

pub struct ContextData {
    pub settings: Settings,
    pub world: WorldHandle,
    pub status: watch::Receiver<ServerStatus>,
    pub shutdown: watch::Receiver<bool>,
}

/// Bind first, then conservatively load or create the operator's stable key.
pub async fn prepare(address: SocketAddr, identity_path: &Path) -> Result<Server> {
    ensure!(
        address.ip().is_loopback(),
        "NetherNet preview requires loopback"
    );
    let listener = TcpListener::bind(address)
        .await
        .context("cannot bind NetherNet TCP signaling listener")?;
    let path = identity_path.to_owned();
    let (identity, media) = tokio::task::spawn_blocking(move || -> Result<_> {
        Ok((load_identity(&path)?, LocalMedia::detect(address.ip())?))
    })
    .await
    .context("NetherNet identity worker failed")??;
    Ok(Server {
        listener,
        identity: Arc::new(identity),
        media: Arc::new(media),
    })
}

fn load_identity(path: &Path) -> Result<ServerIdentity> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "identity must be a regular file"
            );
            ensure!(
                (1..=IDENTITY_BYTES).contains(&metadata.len()),
                "identity file length is invalid"
            );
            let mut text = String::new();
            fs::File::open(path)?
                .take(IDENTITY_BYTES + 1)
                .read_to_string(&mut text)?;
            ensure!(
                text.len() as u64 <= IDENTITY_BYTES,
                "identity exceeds size bound"
            );
            ServerIdentity::from_pem(&text, "sculith.local", SystemTime::now())
                .context("invalid existing NetherNet identity; refusing to replace it")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent)?;
            }
            let identity = ServerIdentity::generate("sculith.local", SystemTime::now())?;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(path)?;
            file.write_all(identity.to_pem()?.as_bytes())?;
            file.sync_all()?;
            Ok(identity)
        }
        Err(error) => Err(error.into()),
    }
}

struct HttpCommand {
    id: u64,
    remote: SocketAddr,
    request: Request<String>,
    reply: oneshot::Sender<Response<String>>,
}

fn response(status: StatusCode) -> Response<String> {
    Response::builder()
        .status(status)
        .body(String::new())
        .expect("fixed response")
}

async fn http_request(
    request: Request<Incoming>,
    remote: SocketAddr,
    id: u64,
    commands: mpsc::Sender<HttpCommand>,
) -> Response<Full<Bytes>> {
    let result = async {
        if request
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|size| size > HTTP_BODY_BYTES as u64)
        {
            return response(StatusCode::PAYLOAD_TOO_LARGE);
        }
        let (parts, mut incoming) = request.into_parts();
        let mut bytes = Vec::new();
        while let Some(frame) = incoming.frame().await {
            let Ok(frame) = frame else {
                return response(StatusCode::BAD_REQUEST);
            };
            if let Some(data) = frame.data_ref() {
                if bytes.len() + data.len() > HTTP_BODY_BYTES {
                    return response(StatusCode::PAYLOAD_TOO_LARGE);
                }
                bytes.extend_from_slice(data);
            }
        }
        let Ok(body) = String::from_utf8(bytes) else {
            return response(StatusCode::BAD_REQUEST);
        };
        let (reply, received) = oneshot::channel();
        if commands
            .send(HttpCommand {
                id,
                remote,
                request: Request::from_parts(parts, body),
                reply,
            })
            .await
            .is_err()
        {
            return response(StatusCode::SERVICE_UNAVAILABLE);
        }
        received
            .await
            .unwrap_or_else(|_| response(StatusCode::SERVICE_UNAVAILABLE))
    };
    let result = tokio::time::timeout(Duration::from_secs(12), result)
        .await
        .unwrap_or_else(|_| response(StatusCode::REQUEST_TIMEOUT));
    result.map(|body| Full::new(Bytes::from(body)))
}

fn server_data(status: &ServerStatus) -> ServerData {
    let mut data = ServerData::new(status.motd.clone(), "Sculith / Bedrock preview".into());
    data.game_type = 1;
    data.player_count = status.online_players as i32;
    data.max_player_count = status.max_players as i32;
    data.protocol_version = V2225::PROTOCOL_VERSION;
    // Patch releases may retain the codec's protocol version.
    data.game_version = crate::protocol::references().bedrock.version;
    data
}

impl Server {
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub async fn serve(self, mut context: ContextData) -> Result<()> {
        let local_ip = self.listener.local_addr()?.ip();
        let mut signaling = HttpSignaler::new(HttpSignalerConfig {
            max_connections_per_address: PER_IP,
            max_pending_joins: HTTP_CONNECTIONS,
            answer_timeout: context.settings.timeout,
            // This loopback preview verifies fingerprint ownership for signed
            // offers. Account authority is still checked by the Bedrock login.
            token_trust: Some(TokenTrust::Any),
            ..Default::default()
        });
        signaling.handle(HttpSignalerInput::SetServerData(Box::new(server_data(
            &context.status.borrow(),
        ))))?;
        let (commands, mut requests) = mpsc::channel::<HttpCommand>(HTTP_CONNECTIONS);
        let http_slots = Arc::new(Semaphore::new(HTTP_CONNECTIONS));
        let incoming_budget = Arc::new(Semaphore::new(QUEUED_BYTES));
        let outgoing_budget = Arc::new(Semaphore::new(QUEUED_BYTES));
        let mut responses: HashMap<u64, oneshot::Sender<Response<String>>> = HashMap::new();
        let mut workers = JoinSet::new();
        let mut applications = JoinSet::new();
        let (ready, mut ready_sessions) = mpsc::channel::<SessionTransport>(MEDIA_CONNECTIONS);
        let mut http_tasks = JoinSet::new();
        let mut peers: HashMap<IpAddr, usize> = HashMap::new();
        let mut next_id = 1u64;
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let result = async { loop {
            tokio::select! {
                changed = context.shutdown.changed() => {
                    if changed.is_err() || *context.shutdown.borrow() { break Ok(()); }
                }
                changed = context.status.changed() => {
                    if changed.is_err() { break Ok(()); }
                    signaling.handle(HttpSignalerInput::SetServerData(Box::new(server_data(&context.status.borrow()))))?;
                }
                accepted = self.listener.accept() => {
                    let (stream, remote) = accepted?;
                    let Ok(permit) = http_slots.clone().try_acquire_owned() else { continue; };
                    let id = next_id;
                    next_id = next_id.checked_add(1).context("HTTP connection IDs exhausted")?;
                    let commands = commands.clone();
                    http_tasks.spawn(async move {
                        let _permit = permit;
                        let service = service_fn(move |request| {
                            let commands = commands.clone();
                            async move { Ok::<_, Infallible>(http_request(request, remote, id, commands).await) }
                        });
                        let _ = tokio::time::timeout(Duration::from_secs(15),
                            http1::Builder::new().keep_alive(false).max_buf_size(8192)
                                .serve_connection(TokioIo::new(stream), service)).await;
                    });
                }
                Some(command) = requests.recv() => {
                    tracing::info!(remote = %command.remote, method = %command.request.method(), "NetherNet signaling request received");
                    responses.insert(command.id, command.reply);
                    signaling.handle(HttpSignalerInput::Connected(command.id, command.remote, Instant::now()))?;
                    signaling.handle(HttpSignalerInput::Request {connection: command.id,
                        request: Box::new(command.request), now: Instant::now()})?;
                }
                Some(result) = workers.join_next(), if !workers.is_empty() => {
                    match result {
                        Ok((ip, result)) => {
                            if let Some(count) = peers.get_mut(&ip) { *count -= 1; if *count == 0 { peers.remove(&ip); } }
                            if let Err(error) = result { tracing::debug!(%ip, %error, "NetherNet session ended"); }
                        }
                        Err(error) => { break Err(error.into()); }
                    }
                }
                Some(result) = http_tasks.join_next(), if !http_tasks.is_empty() => {
                    result.context("NetherNet HTTP task failed")?;
                }
                Some(transport) = ready_sessions.recv() => {
                    let settings = context.settings.clone();
                    let world = context.world.clone();
                    let protection = Protection::WebRtcDtls(transport.identity.clone());
                    applications.spawn(async move {
                        let mut conn = Connection::<V2225, _>::from_transport_conn(transport);
                        conn.max_batch_len = MAX_MESSAGE_BYTES;
                        let result = handle_with_protection(&mut conn, &settings, &world, protection).await;
                        conn.close().await;
                        result
                    });
                }
                Some(result) = applications.join_next(), if !applications.is_empty() => {
                    match result {
                        Ok(Err(error)) => tracing::debug!(%error, "NetherNet Bedrock session ended"),
                        Err(error) => tracing::warn!(%error, "NetherNet game task failed"),
                        _ => {},
                    }
                }
                _ = tick.tick() => {
                    let closed: Vec<_> = responses.iter().filter_map(|(&id, response)| response.is_closed().then_some(id)).collect();
                    for id in closed { responses.remove(&id); signaling.handle(HttpSignalerInput::Closed(id))?; }
                    signaling.handle(HttpSignalerInput::Update(Instant::now()))?;
                }
            }
            while let Some(event) = signaling.poll() {
                match event {
                    HttpSignalerOutput::Response {connection, response, ..} => {
                        tracing::info!(connection, status = response.status().as_u16(),
                            "NetherNet signaling response sent");
                        if let Some(reply) = responses.remove(&connection) { let _ = reply.send(*response); }
                        signaling.handle(HttpSignalerInput::Closed(connection))?;
                    }
                    HttpSignalerOutput::Close(id) => {
                        if let Some(reply) = responses.remove(&id) { let _ = reply.send(response(StatusCode::SERVICE_UNAVAILABLE)); }
                        signaling.handle(HttpSignalerInput::Closed(id))?;
                    }
                    HttpSignalerOutput::Offer(offer) => {
                        let ip = offer.client_address.map(|a| a.ip()).unwrap_or(local_ip);
                        if workers.len() >= MEDIA_CONNECTIONS || peers.get(&ip).copied().unwrap_or(0) >= PER_IP {
                            signaling.handle(HttpSignalerInput::Reject {connection_id: offer.connection_id, reason: RejectReason::Unavailable})?;
                            continue;
                        }
                        let Some(identity) = offer.player.as_deref().cloned().map(Arc::new) else {
                            signaling.handle(HttpSignalerInput::Reject {connection_id: offer.connection_id, reason: RejectReason::Rejected})?;
                            continue;
                        };
                        match answer(&offer, local_ip, &self.identity, context.settings.timeout, &self.media).await {
                            Ok((socket, connection, sdp)) => {
                                signaling.handle(HttpSignalerInput::Answer {connection_id: offer.connection_id, sdp})?;
                                *peers.entry(ip).or_default() += 1;
                                let config = PeerContext { shutdown: context.shutdown.clone(), ready: ready.clone(), media: self.media.clone(), identity,
                                    incoming_budget: incoming_budget.clone(), outgoing_budget: outgoing_budget.clone() };
                                workers.spawn(async move { (ip, drive(socket, connection, config).await) });
                            }
                            Err(error) => {
                                tracing::warn!(%ip, "NetherNet offer rejected");
                                tracing::debug!(%error, "NetherNet answer failure");
                                signaling.handle(HttpSignalerInput::Reject {connection_id: offer.connection_id, reason: RejectReason::Rejected})?;
                            }
                        }
                    }
                    HttpSignalerOutput::Wait(_) => {}
                }
            }
        }}.await;
        // Drain game tasks before returning to main's final world capture.
        applications.abort_all();
        while applications.join_next().await.is_some() {}
        workers.abort_all();
        http_tasks.abort_all();
        while workers.join_next().await.is_some() {}
        while http_tasks.join_next().await.is_some() {}
        result
    }
}

async fn answer(
    offer: &Offer,
    local_ip: IpAddr,
    identity: &ServerIdentity,
    timeout: Duration,
    media: &LocalMedia,
) -> Result<(UdpSocket, Peer, String)> {
    let socket = UdpSocket::bind(SocketAddr::new(media.bind_ip(), 0)).await?;
    // HttpSignaler already verified the offer's signed fingerprint assertion.
    let signal = offer.signal();
    let (mut remote, mut candidates) = Peer::parse_offer(&signal)?;
    let offered_candidates = candidates.len();
    // Only same-machine destinations obtained from the OS are eligible.
    candidates.retain(|candidate| {
        candidate
            .address()
            .parse::<IpAddr>()
            .is_ok_and(|ip| media.allows(ip))
    });
    tracing::debug!(
        offered_candidates,
        same_machine_candidates = candidates.len(),
        "NetherNet offer candidate selection"
    );
    let retained_ports: Vec<_> = candidates
        .iter()
        .map(|candidate| candidate.port())
        .collect();
    // Limit the underlying SCTP stack as well as our application queues, even
    // when a peer advertises zero (unlimited). NetherNet segments are 10 KB.
    const SCTP_LIMIT: u32 = 262_144;
    remote.sctp_max_message_size = match remote.sctp_max_message_size {
        0 => SCTP_LIMIT,
        n => n.min(SCTP_LIMIT),
    };
    let now = Instant::now();
    let advertised = SocketAddr::new(local_ip, socket.local_addr()?.port());
    let (mut session, local) = Session::with_advertised(advertised, media.addresses(), false, now)?;
    session.set_establish_deadline(timeout);
    let (mut connection, signals) = Peer::accept(
        session,
        local,
        &signal,
        remote,
        candidates,
        IceMode::Full,
        now,
    )?;
    // An mDNS host candidate need not name a numeric IP. Try its advertised
    // UDP port only on same-machine addresses, without DNS or forwarded headers.
    if offer
        .client_address
        .is_some_and(|from| from.ip().is_loopback())
    {
        let mut ports = Vec::new();
        for line in offer
            .sdp
            .lines()
            .filter(|line| line.starts_with("a=candidate:"))
        {
            if let Ok(candidate) = nethernet::util::candidate::CandidateLine::try_from(line)
                && candidate.kind() == nethernet::util::candidate::CandidateKind::Host
                && candidate.transport().eq_ignore_ascii_case("udp")
                && candidate.port() != 0
                && !retained_ports.contains(&candidate.port())
                && !ports.contains(&candidate.port())
                && ports.len() < 8
            {
                ports.push(candidate.port());
            }
        }
        tracing::debug!(
            inferred_ports = ports.len(),
            "NetherNet loopback candidate inference"
        );
        for (index, (port, address)) in ports
            .into_iter()
            .flat_map(|port| {
                media
                    .addresses()
                    .iter()
                    .copied()
                    .map(move |address| (port, address))
            })
            .enumerate()
        {
            let candidate = format!(
                "candidate:{} 1 UDP 1677721855 {} {} typ srflx raddr 0.0.0.0 rport 0",
                90_100_000 + index,
                address,
                port
            );
            connection.handle(ConnectionInput::Signal(
                nethernet::protocol::Signal::candidate(
                    offer.connection_id,
                    candidate,
                    offer.network_id.clone(),
                ),
                now,
            ))?;
        }
    }
    let sdp = signals
        .into_iter()
        .next()
        .context("NetherNet answer has no SDP")?
        .data;
    let sdp = identity.augment(&sdp, SystemTime::now())?;
    Ok((socket, connection, sdp))
}

struct Inbound {
    data: Vec<u8>,
    _bytes: OwnedSemaphorePermit,
}
struct SendCommand {
    data: Bytes,
    _bytes: OwnedSemaphorePermit,
    reply: oneshot::Sender<Result<(), String>>,
}

struct SessionTransport {
    remote: SocketAddr,
    incoming: mpsc::Receiver<Inbound>,
    outgoing: mpsc::Sender<SendCommand>,
    budget: Arc<Semaphore>,
    closed: Arc<AtomicBool>,
    identity: Arc<nethernet::identity::PlayerInfo>,
}
impl Drop for SessionTransport {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
    }
}
fn io_error(message: impl Into<String>) -> TransportLayerError {
    std::io::Error::other(message.into()).into()
}

fn candidate_path_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::NetworkUnreachable
            | std::io::ErrorKind::HostUnreachable
    )
}

impl Transport for SessionTransport {
    async fn send(&mut self, batch: &[u8]) -> std::result::Result<(), TransportLayerError> {
        if batch.len() > NETHER_MESSAGE_BYTES || self.closed.load(Ordering::Acquire) {
            return Err(io_error(
                "NetherNet message exceeds limit or session is closed",
            ));
        }
        let permit = self
            .budget
            .clone()
            .acquire_many_owned(batch.len() as u32)
            .await
            .map_err(|_| io_error("NetherNet output budget closed"))?;
        let (reply, received) = oneshot::channel();
        self.outgoing
            .send(SendCommand {
                data: Bytes::copy_from_slice(batch),
                _bytes: permit,
                reply,
            })
            .await
            .map_err(|_| io_error("NetherNet driver stopped"))?;
        received
            .await
            .map_err(|_| io_error("NetherNet send response lost"))?
            .map_err(io_error)
    }
    async fn recv(&mut self) -> std::result::Result<Vec<u8>, TransportLayerError> {
        self.incoming
            .recv()
            .await
            .map(|packet| packet.data)
            .ok_or_else(|| io_error("NetherNet session closed"))
    }
    fn remote_addr(&self) -> SocketAddr {
        self.remote
    }
    fn max_message_len(&self) -> usize {
        NETHER_MESSAGE_BYTES
    }
    async fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
    async fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

struct PeerContext {
    shutdown: watch::Receiver<bool>,
    incoming_budget: Arc<Semaphore>,
    outgoing_budget: Arc<Semaphore>,
    ready: mpsc::Sender<SessionTransport>,
    media: Arc<LocalMedia>,
    identity: Arc<nethernet::identity::PlayerInfo>,
}

async fn drive(socket: UdpSocket, mut peer: Peer, mut context: PeerContext) -> Result<()> {
    let (incoming, receiver) = mpsc::channel(16);
    let mut receiver = Some(receiver);
    let (outgoing, mut sends) = mpsc::channel::<SendCommand>(32);
    let closed = Arc::new(AtomicBool::new(false));
    let mut tick = tokio::time::interval(Duration::from_millis(10));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut buffer = vec![0u8; 65_535];
    let mut observed_stun = false;
    let mut observed_dtls = false;
    let mut observed_path_error = false;
    loop {
        tokio::select! {
            changed = context.shutdown.changed() => {
                if changed.is_err() || *context.shutdown.borrow() { break; }
            }
            packet = socket.recv_from(&mut buffer) => {
                let (size, remote) = match packet {
                    Ok(packet) => packet,
                    Err(error) if candidate_path_error(&error) => {
                        if !observed_path_error {
                            observed_path_error = true;
                            tracing::debug!(%error, "NetherNet candidate path unavailable; continuing ICE");
                        }
                        continue;
                    },
                    Err(error) => return Err(error.into()),
                };
                if !context.media.allows(remote.ip()) { continue; }
                let stun = size >= 20 && buffer[0] & 0xc0 == 0
                    && buffer[4..8] == [0x21, 0x12, 0xa4, 0x42];
                let dtls = size >= 13 && (20..=63).contains(&buffer[0]);
                if stun && !observed_stun {
                    observed_stun = true;
                    tracing::debug!(%remote, "NetherNet first ICE/STUN datagram received");
                }
                if dtls && !observed_dtls {
                    observed_dtls = true;
                    tracing::debug!(%remote, "NetherNet first DTLS datagram received");
                }
                peer.handle(ConnectionInput::Packet(buffer[..size].into(), remote, Instant::now()))?;
            }
            _ = tick.tick() => {
                if closed.load(Ordering::Acquire) { break; }
                peer.handle(ConnectionInput::Timeout(Instant::now()))?;
            }
            Some(command) = sends.recv() => {
                let result = peer.handle(ConnectionInput::Send(Channel::Reliable, command.data, Instant::now()));
                let _ = command.reply.send(result.map_err(|e| e.to_string()));
            }
        }
        while let Some(output) = peer.poll() {
            match output {
                SessionOutput::Send(data, remote) => {
                    ensure!(
                        context.media.allows(remote.ip()),
                        "NetherNet candidate outside same-machine policy"
                    );
                    if let Err(error) = socket.send_to(&data, remote).await {
                        if !candidate_path_error(&error) {
                            return Err(error.into());
                        }
                        if !observed_path_error {
                            observed_path_error = true;
                            tracing::debug!(%error, "NetherNet candidate path unavailable; continuing ICE");
                        }
                    }
                }
                SessionOutput::Event(SessionEvent::Ready) => {
                    let Some(receiver) = receiver.take() else {
                        continue;
                    };
                    let remote = peer
                        .remote_addr()
                        .context("ready NetherNet peer has no address")?;
                    tracing::info!(%remote, "NetherNet WebRTC transport connected");
                    let transport = SessionTransport {
                        remote,
                        incoming: receiver,
                        outgoing: outgoing.clone(),
                        budget: context.outgoing_budget.clone(),
                        closed: closed.clone(),
                        identity: context.identity.clone(),
                    };
                    context
                        .ready
                        .try_send(transport)
                        .map_err(|_| anyhow::anyhow!("NetherNet ready queue exhausted"))?;
                }
                SessionOutput::Event(SessionEvent::Failed) => {
                    anyhow::bail!("NetherNet ICE/DTLS/SCTP establishment failed")
                }
                SessionOutput::Message(_, data) => {
                    ensure!(
                        data.len() <= NETHER_MESSAGE_BYTES,
                        "NetherNet incoming message too large"
                    );
                    let reservation = context
                        .incoming_budget
                        .clone()
                        .try_acquire_many_owned(data.len() as u32)
                        .context("NetherNet incoming byte budget exhausted")?;
                    incoming
                        .try_send(Inbound {
                            data,
                            _bytes: reservation,
                        })
                        .map_err(|_| anyhow::anyhow!("NetherNet incoming queue exhausted"))?;
                }
                SessionOutput::Wait(_) => {}
            }
        }
    }
    closed.store(true, Ordering::Release);
    Ok(())
}

#[cfg(test)]
mod tests;
