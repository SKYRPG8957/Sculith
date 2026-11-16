//! RakNet discovery, plus the opt-in V2225 encrypted login and read-only spawn.

pub mod catalog;
mod chunk;
mod commands;
mod input;
mod inventory;
pub mod nethernet;
mod output;
mod peers;
pub mod session;
mod spawn;
mod transport;
pub use transport::serve as serve_sessions;

use std::{
    collections::HashMap,
    net::IpAddr,
    time::{Duration, Instant},
};

use anyhow::Result;
use sculith_core::ServerStatus;
use tokio::{net::UdpSocket, sync::watch};

pub const MAGIC: [u8; 16] = [
    0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34, 0x56, 0x78,
];
const MAX_TRACKED_IPS: usize = 4096;

pub async fn serve(
    socket: UdpSocket,
    status: watch::Receiver<ServerStatus>,
    guid: u64,
) -> Result<()> {
    let address = socket.local_addr()?;
    let (port4, port6) = if address.is_ipv4() {
        (address.port(), 0)
    } else {
        (0, address.port())
    };
    let reference = super::references().bedrock;
    let mut buffer = [0; 2048];
    let mut limiter = RateLimiter::new(Instant::now());
    loop {
        let (length, remote) = socket.recv_from(&mut buffer).await?;
        let ping = &buffer[..length];
        if !valid_ping(ping) || !limiter.allow(remote.ip(), Instant::now()) {
            continue;
        }
        let snapshot = status.borrow().clone();
        let pong = make_pong(ping, &snapshot, guid, &reference, port4, port6);
        if let Err(error) = socket.send_to(&pong, remote).await {
            tracing::debug!(%remote, %error, "Bedrock discovery reply failed");
        }
    }
}

fn valid_ping(ping: &[u8]) -> bool {
    ping.len() == 33 && matches!(ping[0], 0x01 | 0x02) && ping[9..25] == MAGIC
}

fn make_pong(
    ping: &[u8],
    status: &ServerStatus,
    guid: u64,
    reference: &super::Reference,
    port4: u16,
    port6: u16,
) -> Vec<u8> {
    let stage = match status.stage.as_str() {
        "bedrock-spawn-preview" => "Sculith / Bedrock spawn preview",
        "shared-world-preview" => "Sculith / shared-world preview",
        _ => "Sculith / discovery only",
    };
    let game_mode = if matches!(
        status.stage.as_str(),
        "bedrock-spawn-preview" | "shared-world-preview"
    ) {
        "Creative"
    } else {
        "Survival"
    };
    let text = format!(
        "MCPE;{};{};{};{};{};{};{};{};1;{};{};",
        status.motd,
        reference.protocol,
        reference.version,
        status.online_players,
        status.max_players,
        guid,
        stage,
        game_mode,
        port4,
        port6
    );
    let mut packet = Vec::with_capacity(35 + text.len());
    packet.push(0x1c);
    packet.extend_from_slice(&ping[1..9]);
    packet.extend_from_slice(&guid.to_be_bytes());
    packet.extend_from_slice(&MAGIC);
    packet.extend_from_slice(&(text.len() as u16).to_be_bytes());
    packet.extend_from_slice(text.as_bytes());
    packet
}

struct Bucket {
    updated: Instant,
    tokens: f64,
}

struct RateLimiter {
    buckets: HashMap<IpAddr, Bucket>,
    cleaned: Instant,
    window: Instant,
    remaining: u16,
}

impl RateLimiter {
    fn new(now: Instant) -> Self {
        Self {
            buckets: HashMap::new(),
            cleaned: now,
            window: now,
            remaining: 512,
        }
    }

    fn allow(&mut self, ip: IpAddr, now: Instant) -> bool {
        if now.duration_since(self.window) >= Duration::from_secs(1) {
            self.window = now;
            self.remaining = 512;
        }
        if self.remaining == 0 {
            return false;
        }
        if now.duration_since(self.cleaned) >= Duration::from_secs(60) {
            self.buckets
                .retain(|_, bucket| now.duration_since(bucket.updated) < Duration::from_secs(60));
            self.cleaned = now;
        }
        if self.buckets.len() >= MAX_TRACKED_IPS && !self.buckets.contains_key(&ip) {
            return false;
        }
        let bucket = self.buckets.entry(ip).or_insert(Bucket {
            updated: now,
            tokens: 10.0,
        });
        bucket.tokens =
            (bucket.tokens + now.duration_since(bucket.updated).as_secs_f64() * 5.0).min(10.0);
        bucket.updated = now;
        if bucket.tokens < 1.0 {
            return false;
        }
        bucket.tokens -= 1.0;
        self.remaining -= 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ping() -> Vec<u8> {
        let mut packet = vec![1];
        packet.extend_from_slice(&123_456u64.to_be_bytes());
        packet.extend_from_slice(&MAGIC);
        packet.extend_from_slice(&42u64.to_be_bytes());
        packet
    }

    #[test]
    fn ping_requires_exact_layout_and_magic() {
        let packet = ping();
        assert!(valid_ping(&packet));
        for length in 0..packet.len() {
            assert!(!valid_ping(&packet[..length]));
        }
        let mut invalid = packet.clone();
        invalid[9] ^= 0xff;
        assert!(!valid_ping(&invalid));
        let mut invalid = packet.clone();
        invalid.push(0);
        assert!(!valid_ping(&invalid));
    }

    #[test]
    fn per_ip_bucket_recovers_and_table_stays_bounded() {
        let now = Instant::now();
        let mut limiter = RateLimiter::new(now);
        let ip = "127.0.0.1".parse().unwrap();
        for _ in 0..10 {
            assert!(limiter.allow(ip, now));
        }
        assert!(!limiter.allow(ip, now));
        assert!(limiter.allow(ip, now + Duration::from_secs(1)));
        for index in 1..5000u32 {
            limiter.allow(
                IpAddr::V4(index.into()),
                now + Duration::from_secs(1 + index as u64 / 400),
            );
        }
        assert!(limiter.buckets.len() <= MAX_TRACKED_IPS);
    }

    #[tokio::test]
    async fn pong_crosses_real_udp_and_observes_shared_status_updates() {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let (sender, receiver) = watch::channel(ServerStatus::new("old".into(), 24).unwrap());
        let task = tokio::spawn(serve(socket, receiver, 123));
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender.send_replace(ServerStatus::new("공통 MOTD".into(), 24).unwrap());
        let ping = ping();
        client.send_to(&ping, address).await.unwrap();
        let mut buffer = [0; 2048];
        let (length, _) =
            tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(buffer[0], 0x1c);
        assert_eq!(&buffer[1..9], &ping[1..9]);
        assert_eq!(u64::from_be_bytes(buffer[9..17].try_into().unwrap()), 123);
        assert_eq!(&buffer[17..33], &MAGIC);
        let text_length = u16::from_be_bytes(buffer[33..35].try_into().unwrap()) as usize;
        assert_eq!(text_length, length - 35);
        let fields: Vec<_> = std::str::from_utf8(&buffer[35..length])
            .unwrap()
            .split(';')
            .collect();
        assert_eq!(fields[0], "MCPE");
        assert_eq!(fields[1], "공통 MOTD");
        assert_eq!(fields[4], "0");
        assert_eq!(fields[5], "24");
        assert_eq!(fields[7], "Sculith / discovery only");
        assert_eq!(fields[10], address.port().to_string());
        task.abort();
        let _ = task.await;
    }

    #[test]
    fn shared_world_discovery_advertises_its_actual_native_creative_mode() {
        let mut status = ServerStatus::new("shared".into(), 20).unwrap();
        status.stage = "shared-world-preview".into();
        let bytes = make_pong(
            &ping(),
            &status,
            123,
            &super::super::references().bedrock,
            19132,
            0,
        );
        let fields: Vec<_> = std::str::from_utf8(&bytes[35..])
            .unwrap()
            .split(';')
            .collect();
        assert_eq!(fields[7], "Sculith / shared-world preview");
        assert_eq!(fields[8], "Creative");
    }
}
