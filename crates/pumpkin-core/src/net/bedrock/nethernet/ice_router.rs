use std::{
    collections::HashMap,
    io::Error,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Weak},
};

use super::direct_udp::DirectUdp;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::trace;

use super::discovery::{
    Packet as DiscoveryPacket, decode_packet, encode_discovery_response, encode_message,
};
use super::ice_socket::IceSocket;
use crate::server::Server;

enum Command {
    Register {
        ufrag: String,
        internal: SocketAddr,
        candidate: Option<SocketAddr>,
        ready: oneshot::Sender<()>,
    },
    Remove {
        ufrag: String,
        internal: SocketAddr,
    },
}

struct Route {
    ufrag: String,
    remote: Option<SocketAddr>,
}

/// Routes every direct-IP peer through Sculith's one public Bedrock UDP socket.
pub(super) struct IceRouter {
    internal_addr: SocketAddr,
    public_addr: SocketAddr,
    commands: mpsc::UnboundedSender<Command>,
    pub(super) direct_udp: Arc<DirectUdp>,
    _stop: DropGuard,
}

pub(super) struct Registration {
    ufrag: String,
    internal: SocketAddr,
    commands: mpsc::UnboundedSender<Command>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Remove {
            ufrag: self.ufrag.clone(),
            internal: self.internal,
        });
    }
}

impl IceRouter {
    pub(super) async fn bind(
        public: IceSocket,
        server: Weak<Server>,
        server_guid: u64,
        stop: CancellationToken,
    ) -> Result<Self, Error> {
        let public_addr = public.local_addr()?;
        let internal = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?);
        let internal_addr = internal.local_addr()?;
        let (commands, receiver) = mpsc::unbounded_channel();
        let stop = stop.child_token();
        let guard = stop.clone().drop_guard();
        let direct_udp = Arc::new(DirectUdp::new(public.clone(), internal_addr));
        let ingress = direct_udp.clone();
        let runtime = tokio::runtime::Handle::current();
        tokio::spawn(send_outbound(
            public.clone(),
            internal.clone(),
            direct_udp.clone(),
            stop.clone(),
        ));
        // One ingress thread per listener keeps a full gameplay executor from
        // leaving a simultaneous arrival burst in the kernel's receive queue.
        std::thread::Builder::new()
            .name("sculith-bedrock-udp".into())
            .spawn(move || {
                runtime.block_on(run(
                    public,
                    internal,
                    receiver,
                    server,
                    server_guid,
                    stop,
                    ingress,
                ));
            })?;
        Ok(Self {
            internal_addr,
            public_addr,
            commands,
            direct_udp,
            _stop: guard,
        })
    }

    pub(super) const fn internal_addr(&self) -> SocketAddr {
        self.internal_addr
    }

    pub(super) const fn public_addr(&self) -> SocketAddr {
        self.public_addr
    }

    pub(super) async fn register(
        &self,
        ufrag: String,
        internal: SocketAddr,
        candidates: Vec<SocketAddr>,
    ) -> Result<Registration, String> {
        let (ready, registered) = oneshot::channel();
        self.commands
            .send(Command::Register {
                ufrag: ufrag.clone(),
                internal,
                candidate: candidates.first().copied(),
                ready,
            })
            .map_err(|_| "ICE listener closed".to_owned())?;
        registered
            .await
            .map_err(|_| "ICE listener closed".to_owned())?;
        Ok(Registration {
            ufrag,
            internal,
            commands: self.commands.clone(),
        })
    }
}

async fn send_outbound(
    public: IceSocket,
    socket: Arc<UdpSocket>,
    direct: Arc<DirectUdp>,
    stop: CancellationToken,
) {
    let mut buffer = vec![0; usize::from(u16::MAX)];
    loop {
        let result = tokio::select! {
            biased;
            () = stop.cancelled() => return,
            result = socket.recv_from(&mut buffer) => result,
        };
        let Ok((len, internal)) = result else {
            continue;
        };
        if let Some(remote) = direct.target(internal)
            && let Err(error) = public.send_to(&buffer[..len], remote).await
        {
            trace!(%internal, %remote, %error, "Outbound ICE relay failed");
        }
    }
}

async fn run(
    public: IceSocket,
    internal: Arc<UdpSocket>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    server: Weak<Server>,
    server_guid: u64,
    stop: CancellationToken,
    direct: Arc<DirectUdp>,
) {
    let advertisement_id = rand::random();
    let mut routes = HashMap::<SocketAddr, Route>::new();
    let mut by_ufrag = HashMap::<String, SocketAddr>::new();
    let mut by_remote = HashMap::<SocketAddr, SocketAddr>::new();
    let mut buffer = vec![0; usize::from(u16::MAX)];
    loop {
        let first = tokio::select! {
            biased;
            () = stop.cancelled() => return,
            command = commands.recv() => {
                match command {
                    Some(Command::Register { ufrag, internal, candidate, ready }) => {
                        if let Some(old) = routes.insert(internal, Route {ufrag: ufrag.clone(), remote: None}) {
                            by_ufrag.remove(&old.ufrag);
                            by_remote.retain(|_, route| *route != internal);
                        }
                        direct.set_target(internal, candidate);
                        by_ufrag.insert(ufrag, internal);
                        let _ = ready.send(());
                    }
                    Some(Command::Remove { ufrag, internal }) => {
                        if routes.get(&internal).is_some_and(|route| route.ufrag == ufrag) {
                            routes.remove(&internal);
                            by_ufrag.remove(&ufrag);
                            by_remote.retain(|_, route| *route != internal);
                            direct.set_target(internal, None);
                        }
                    }
                    None => return,
                }
                continue;
            }
            result = public.recv_from(&mut buffer) => result,
        };
        let mut result = first;
        for index in 0..16 {
            let Ok((len, remote)) = result else { break };
            let packet = &buffer[..len];
            let destination = stun_username(packet)
                .and_then(|username| {
                    username
                        .split(':')
                        .find_map(|frag| by_ufrag.get(frag).copied())
                })
                .or_else(|| by_remote.get(&remote).copied());
            if let Some(destination) = destination {
                if let Some(route) = routes.get_mut(&destination)
                    && route.remote != Some(remote)
                {
                    if let Some(previous) = route.remote.replace(remote)
                        && by_remote.get(&previous) == Some(&destination)
                    {
                        by_remote.remove(&previous);
                    }
                    direct.set_target(destination, Some(remote));
                    by_remote.insert(remote, destination);
                }
                if !direct.deliver(destination, packet)
                    && let Err(error) = internal.send_to(packet, destination).await
                {
                    trace!(%remote, %destination, %error, "Inbound ICE relay failed");
                }
            } else {
                handle_unroutable(
                    packet,
                    remote,
                    &public,
                    &server,
                    server_guid,
                    advertisement_id,
                )
                .await;
            }
            if index < 15 {
                result = public.try_recv_from(&mut buffer);
            }
        }
    }
}

async fn handle_unroutable(
    packet: &[u8],
    remote: SocketAddr,
    public: &IceSocket,
    server: &Weak<Server>,
    server_guid: u64,
    advertisement_id: u64,
) {
    let Some(discovery_packet) = decode_packet(packet) else {
        trace!(%remote, length = packet.len(), "Dropped unroutable NetherNet ICE packet");
        return;
    };
    match discovery_packet {
        DiscoveryPacket::Request => {
            if let Some(server) = server.upgrade()
                && let Ok(response) =
                    encode_discovery_response(&server, server_guid, advertisement_id)
            {
                trace!(%remote, "Received NetherNet discovery request on ICE UDP socket");
                let _ = public.send_to(&response, remote).await;
            }
        }
        DiscoveryPacket::Message {
            sender_id,
            recipient_id,
            data,
        } if recipient_id == server_guid && data.eq_ignore_ascii_case("ping") => {
            trace!(%remote, sender_id, "Received NetherNet Ping signal on ICE UDP socket");
            if let Ok(response) = encode_message(server_guid, sender_id, "Pong") {
                let _ = public.send_to(&response, remote).await;
            }
        }
        DiscoveryPacket::Message { .. } => {}
    }
}

fn stun_username(packet: &[u8]) -> Option<&str> {
    if packet.len() < 20 || packet.get(4..8)? != [0x21, 0x12, 0xa4, 0x42] {
        return None;
    }
    let attributes_length = usize::from(u16::from_be_bytes(packet.get(2..4)?.try_into().ok()?));
    let end = 20usize.checked_add(attributes_length)?;
    if end > packet.len() {
        return None;
    }

    let mut offset = 20usize;
    while offset.checked_add(4)? <= end {
        let attribute = u16::from_be_bytes(packet.get(offset..offset + 2)?.try_into().ok()?);
        let length = usize::from(u16::from_be_bytes(
            packet.get(offset + 2..offset + 4)?.try_into().ok()?,
        ));
        let value_start = offset + 4;
        let value_end = value_start.checked_add(length)?;
        if value_end > end {
            return None;
        }
        if attribute == 0x0006 {
            return std::str::from_utf8(packet.get(value_start..value_end)?).ok();
        }
        offset = value_start.checked_add(length.next_multiple_of(4))?;
    }
    None
}

pub(super) fn proxy_offer(sdp: &str, proxy: SocketAddr) -> (String, Vec<SocketAddr>) {
    let mut candidates = Vec::new();
    let sdp = rewrite_sdp_candidates(sdp, |fields| {
        if is_component_one_udp(fields) {
            if let Some(candidate) = candidate_address(fields) {
                candidates.push(candidate);
            }
            fields[4] = proxy.ip().to_string();
            fields[5] = proxy.port().to_string();
        }
    });
    (sdp, candidates)
}

pub(super) fn proxy_answer(
    sdp: &str,
    public_ip: Option<IpAddr>,
    public_port: u16,
) -> Result<(String, String, SocketAddr), String> {
    let ufrag = sdp
        .lines()
        .find_map(|line| line.strip_prefix("a=ice-ufrag:"))
        .map(str::to_owned)
        .ok_or_else(|| "WebRTC answer has no ICE username fragment".to_string())?;
    let candidate = sdp
        .lines()
        .filter_map(|line| line.strip_prefix("a=candidate:"))
        .map(|candidate| {
            candidate
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .find(|fields| is_component_one_udp(fields))
        .and_then(|fields| candidate_address(&fields))
        .ok_or_else(|| "WebRTC answer has no UDP host candidate".to_string())?;
    let internal = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), candidate.port());
    let answer = rewrite_sdp_candidates(sdp, |fields| {
        if is_component_one_udp(fields) {
            if let Some(public_ip) = public_ip {
                fields[4] = public_ip.to_string();
            }
            fields[5] = public_port.to_string();
        }
    });
    Ok((answer, ufrag, internal))
}

fn rewrite_sdp_candidates(sdp: &str, mut rewrite: impl FnMut(&mut Vec<String>)) -> String {
    let mut rewritten = String::with_capacity(sdp.len());
    for line in sdp.lines() {
        if let Some(candidate) = line.strip_prefix("a=candidate:") {
            let mut fields = candidate
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            rewrite(&mut fields);
            rewritten.push_str("a=candidate:");
            rewritten.push_str(&fields.join(" "));
        } else {
            rewritten.push_str(line);
        }
        rewritten.push_str("\r\n");
    }
    rewritten
}

fn is_component_one_udp(fields: &[String]) -> bool {
    fields.len() >= 8 && fields[1] == "1" && fields[2].eq_ignore_ascii_case("udp")
}

fn candidate_address(fields: &[String]) -> Option<SocketAddr> {
    Some(SocketAddr::new(
        fields.get(4)?.parse().ok()?,
        fields.get(5)?.parse().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dropping_listener_releases_ports_with_a_live_registration() {
        let public = IceSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let router = IceRouter::bind(public, Weak::new(), 1, CancellationToken::new())
            .await
            .unwrap();
        let public = router.public_addr();
        let internal = router.internal_addr();
        let _registration = router
            .register(
                "peer".into(),
                SocketAddr::from(([127, 0, 0, 1], 1)),
                Vec::new(),
            )
            .await
            .unwrap();
        drop(router);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if UdpSocket::bind(public).await.is_ok() && UdpSocket::bind(internal).await.is_ok()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn reads_stun_username() {
        let username = b"server:client";
        let mut packet = vec![0, 1, 0, 20, 0x21, 0x12, 0xa4, 0x42];
        packet.extend_from_slice(&[0; 12]);
        packet.extend_from_slice(&0x0006u16.to_be_bytes());
        packet.extend_from_slice(&(username.len() as u16).to_be_bytes());
        packet.extend_from_slice(username);
        packet.resize(packet.len().next_multiple_of(4), 0);
        assert_eq!(stun_username(&packet), Some("server:client"));
    }

    #[test]
    fn proxies_offer_and_answer_candidates() {
        let offer =
            "v=0\r\na=ice-ufrag:client\r\na=candidate:1 1 udp 123 192.168.1.7 50000 typ host\r\n";
        let proxy = "127.0.0.1:40000".parse().unwrap();
        let (offer, candidates) = proxy_offer(offer, proxy);
        assert!(offer.contains("127.0.0.1 40000 typ host"));
        assert_eq!(
            candidates,
            ["192.168.1.7:50000".parse::<SocketAddr>().unwrap()]
        );

        let answer =
            "v=0\r\na=ice-ufrag:server\r\na=candidate:2 1 udp 456 192.168.1.8 51000 typ host\r\n";
        let (answer, ufrag, internal) =
            proxy_answer(answer, Some("203.0.113.7".parse().unwrap()), 19132).unwrap();
        assert!(answer.contains("203.0.113.7 19132 typ host"));
        assert_eq!(ufrag, "server");
        assert_eq!(internal, "127.0.0.1:51000".parse::<SocketAddr>().unwrap());
    }
}
