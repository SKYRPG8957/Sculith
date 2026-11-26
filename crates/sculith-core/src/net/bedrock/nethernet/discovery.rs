use std::{
    collections::HashMap,
    io::{Error, ErrorKind},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, LazyLock},
    time::Duration,
};

use aes::{
    Aes256, Block,
    cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit},
};
use hmac::{Hmac, Mac};
use sculith_protocol::{
    codec::{var_int::VarInt, var_uint::VarUInt},
    serial::PacketWrite,
};
use sculith_world::{CURRENT_BEDROCK_MC_PROTOCOL, CURRENT_BEDROCK_MC_VERSION};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, trace, warn};
use webrtc::peer_connection::RTCIceCandidateInit;

use super::{ice_socket::IceSocket, peer::negotiate, state::NetherNetState};
use crate::server::Server;

const DISCOVERY_PORT: u16 = 7551;
const CHECKSUM_SIZE: usize = 32;
const HEADER_SIZE: usize = 18;
const MESSAGE_HEADER_SIZE: usize = 12;
// The client caps both declared lengths but sends the entire encrypted SDP.
const MAX_DECLARED_MESSAGE_LENGTH: usize = 1140;
const REQUEST_PACKET: u16 = 0;
const RESPONSE_PACKET: u16 = 1;
const MESSAGE_PACKET: u16 = 2;
const SERVER_DATA_VERSION: u8 = 7;

type ConnectionKey = (u64, u64);

static KEY: LazyLock<[u8; 32]> =
    LazyLock::new(|| Sha256::digest(0xdeadbeefu64.to_le_bytes()).into());

pub struct NetherNetDiscovery {
    socket: IceSocket,
    network_id: u64,
    advertisement_id: u64,
}

pub(super) struct LanSignal {
    pub socket: IceSocket,
    pub address: SocketAddr,
    pub sender_id: u64,
    pub data: String,
}

#[derive(Clone, Default)]
pub(super) struct LanSignaling {
    candidates: Arc<Mutex<HashMap<ConnectionKey, mpsc::Sender<RTCIceCandidateInit>>>>,
}

impl NetherNetDiscovery {
    pub async fn bind(address: SocketAddr, network_id: u64) -> Result<Self, Error> {
        let ip = match address.ip() {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(_) => Ipv4Addr::UNSPECIFIED,
        };
        let socket = IceSocket::bind(SocketAddr::from((ip, DISCOVERY_PORT))).await?;
        Ok(Self {
            socket,
            network_id,
            advertisement_id: rand::random(),
        })
    }

    pub(super) async fn receive(
        &self,
        state: &NetherNetState,
        signaling: &LanSignaling,
        buffer: &mut [u8],
    ) -> Result<(), Error> {
        let (length, address) = self.socket.recv_from(buffer).await?;
        match decode_packet(&buffer[..length]) {
            Some(Packet::Request) => {
                trace!("Received NetherNet LAN discovery request from {address}");
                if let Some(server) = state.server.upgrade() {
                    self.advertise(&server, address).await?;
                }
            }
            Some(Packet::Message {
                sender_id,
                recipient_id,
                data,
            }) if recipient_id == self.network_id => {
                trace!(
                    sender_id,
                    recipient_id,
                    signal = data
                        .split_once(' ')
                        .map_or(data.as_str(), |(signal, _)| signal),
                    "Received NetherNet LAN signal from {address}"
                );
                signaling
                    .handle(
                        state,
                        LanSignal {
                            socket: self.socket.clone(),
                            address,
                            sender_id,
                            data,
                        },
                    )
                    .await;
            }
            Some(Packet::Message {
                sender_id,
                recipient_id,
                ..
            }) => trace!(
                sender_id,
                recipient_id,
                expected_recipient_id = self.network_id,
                "Ignored NetherNet LAN signal for another server"
            ),
            None => trace!("Ignored invalid {length}-byte NetherNet LAN packet from {address}"),
        }
        Ok(())
    }

    async fn advertise(&self, server: &Server, address: SocketAddr) -> Result<(), Error> {
        let response = encode_discovery_response(server, self.network_id, self.advertisement_id)?;
        self.socket.send_to(&response, address).await?;
        trace!(
            %address,
            network_id = self.network_id,
            advertisement_id = format_args!("{:016x}", self.advertisement_id),
            version = SERVER_DATA_VERSION,
            online_mode = server.advanced_config.networking.bedrock.online_mode,
            "Sent NetherNet LAN discovery response"
        );
        Ok(())
    }

    pub fn local_addr(&self) -> Result<SocketAddr, Error> {
        self.socket.local_addr()
    }
}

impl LanSignaling {
    pub(super) async fn receive_game_port(
        self,
        state: NetherNetState,
        mut incoming: mpsc::Receiver<LanSignal>,
    ) {
        loop {
            tokio::select! {
                () = state.stop_token.cancelled() => break,
                signal = incoming.recv() => {
                    let Some(signal) = signal else { break };
                    self.handle(&state, signal).await;
                }
            }
        }
    }

    pub(super) async fn handle(&self, state: &NetherNetState, signal: LanSignal) {
        let LanSignal {
            socket,
            address,
            sender_id,
            data,
        } = signal;
        let Some(server) = state.server.upgrade() else {
            return;
        };
        let network_id = server.server_guid;
        if data.eq_ignore_ascii_case("ping") {
            trace!(%address, sender_id, "Responding to NetherNet LAN Ping signal");
            if let Ok(response) = encode_message(network_id, sender_id, "Pong")
                && let Err(error) = socket.send_to(&response, address).await
            {
                warn!("Failed to send NetherNet LAN Pong signal to {address}: {error}");
            }
            return;
        }
        let mut parts = data.splitn(3, ' ');
        let (Some(signal_type), Some(connection_id), Some(data)) =
            (parts.next(), parts.next(), parts.next())
        else {
            trace!(%address, sender_id, "Ignored empty or malformed NetherNet LAN signal");
            return;
        };
        let Ok(connection_id) = connection_id.parse::<u64>() else {
            trace!(%address, sender_id, "Ignored NetherNet LAN signal with invalid connection ID");
            return;
        };
        let key = (sender_id, connection_id);

        match signal_type {
            "CONNECTREQUEST" => {
                trace!(%address, sender_id, connection_id, "Accepted NetherNet LAN connection request");
                let (candidate_sender, candidate_receiver) = mpsc::channel(64);
                self.candidates.lock().await.insert(key, candidate_sender);

                let state = state.clone();
                let candidates = self.candidates.clone();
                let offer = data.to_owned();
                tokio::spawn(async move {
                    let signal = match Box::pin(negotiate(
                        &state,
                        address,
                        &offer,
                        Some(candidate_receiver),
                    ))
                    .await
                    {
                        Ok((answer, _session)) => {
                            format!("CONNECTRESPONSE {connection_id} {answer}")
                        }
                        Err(error) => {
                            warn!("NetherNet LAN negotiation with {address} failed: {error}");
                            format!("CONNECTERROR {connection_id} 11")
                        }
                    };
                    match encode_message(network_id, sender_id, &signal) {
                        Ok(response) => {
                            if let Err(error) = socket.send_to(&response, address).await {
                                warn!("Failed to send NetherNet LAN signal to {address}: {error}");
                            } else {
                                trace!(%address, sender_id, connection_id, "Sent NetherNet LAN connection response");
                            }
                        }
                        Err(error) => warn!("Failed to encode NetherNet LAN signal: {error}"),
                    }
                    tokio::select! {
                        () = state.stop_token.cancelled() => {},
                        () = tokio::time::sleep(Duration::from_secs(30)) => {},
                    }
                    candidates.lock().await.remove(&key);
                });
            }
            "CANDIDATEADD" => {
                let candidate =
                    serde_json::from_str(data).unwrap_or_else(|_| RTCIceCandidateInit {
                        candidate: data.to_owned(),
                        ..Default::default()
                    });
                if let Some(sender) = self.candidates.lock().await.get(&key) {
                    let _ = sender.try_send(candidate);
                    trace!(%address, sender_id, connection_id, "Forwarded NetherNet LAN ICE candidate");
                } else {
                    trace!(%address, sender_id, connection_id, "Ignored NetherNet LAN ICE candidate for unknown connection");
                }
            }
            signal_type => debug!("Ignoring NetherNet LAN signal {signal_type}"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Packet {
    Request,
    Message {
        sender_id: u64,
        recipient_id: u64,
        data: String,
    },
}

pub(super) fn decode_packet(data: &[u8]) -> Option<Packet> {
    let encrypted = data.get(CHECKSUM_SIZE..)?;
    if encrypted.is_empty() || !encrypted.len().is_multiple_of(16) {
        return None;
    }

    let mut payload = encrypted.to_vec();
    decrypt(&mut payload)?;
    let padding = usize::from(*payload.last()?);
    if padding == 0
        || padding > 16
        || payload.len() < padding
        || payload[payload.len() - padding..]
            .iter()
            .any(|byte| usize::from(*byte) != padding)
    {
        return None;
    }
    payload.truncate(payload.len() - padding);

    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(KEY.as_slice()).ok()?;
    mac.update(&payload);
    mac.verify_slice(data.get(..CHECKSUM_SIZE)?).ok()?;

    let declared_length = usize::from(u16::from_le_bytes(payload.get(..2)?.try_into().ok()?));
    if payload.get(12..20)? != [0; 8] {
        return None;
    }
    let packet_id = u16::from_le_bytes(payload.get(2..4)?.try_into().ok()?);
    let sender_id = u64::from_le_bytes(payload.get(4..12)?.try_into().ok()?);
    let body = payload.get(20..)?;
    match packet_id {
        REQUEST_PACKET
            if body.is_empty() && [payload.len(), HEADER_SIZE].contains(&declared_length) =>
        {
            Some(Packet::Request)
        }
        MESSAGE_PACKET => {
            let recipient_id = u64::from_le_bytes(body.get(..8)?.try_into().ok()?);
            let length = u32::from_le_bytes(body.get(8..12)?.try_into().ok()?) as usize;
            let data = body.get(MESSAGE_HEADER_SIZE..)?;
            let maximum = data.len().min(MAX_DECLARED_MESSAGE_LENGTH);
            if ![data.len(), maximum].contains(&length) {
                return None;
            }
            let declared_with_header = 2 + HEADER_SIZE + MESSAGE_HEADER_SIZE + length;
            if ![declared_with_header, declared_with_header - 2].contains(&declared_length) {
                return None;
            }
            Some(Packet::Message {
                sender_id,
                recipient_id,
                data: String::from_utf8(data.to_vec()).ok()?,
            })
        }
        _ => None,
    }
}

pub(super) fn encode_discovery_response(
    server: &Server,
    network_id: u64,
    advertisement_id: u64,
) -> Result<Vec<u8>, Error> {
    let players = u32::try_from(server.get_player_count()).unwrap_or(u32::MAX);
    let game_mode = server
        .defaultgamemode
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .gamemode as u8;
    let config = &server.advanced_config.networking.bedrock;
    encode_response(
        network_id,
        advertisement_id,
        &server.advanced_config.networking.bedrock.motd,
        &server.basic_config.default_level_name,
        game_mode,
        players,
        server.advanced_config.networking.bedrock.max_players,
        server.basic_config.hardcore,
        !(config.online_mode && config.authentication.enabled),
    )
}

#[allow(clippy::too_many_arguments)]
fn encode_response(
    network_id: u64,
    advertisement_id: u64,
    server_name: &str,
    level_name: &str,
    game_mode: u8,
    player_count: u32,
    max_player_count: u32,
    hardcore: bool,
    accepts_self_signed: bool,
) -> Result<Vec<u8>, Error> {
    let mut server_data = vec![SERVER_DATA_VERSION];
    push_string(&mut server_data, server_name)?;
    VarInt(CURRENT_BEDROCK_MC_PROTOCOL as i32).write(&mut server_data)?;
    push_string(&mut server_data, CURRENT_BEDROCK_MC_VERSION)?;
    push_string(&mut server_data, level_name)?;
    VarInt(i32::try_from(player_count).unwrap_or(i32::MAX)).write(&mut server_data)?;
    VarInt(i32::try_from(max_player_count).unwrap_or(i32::MAX)).write(&mut server_data)?;
    VarInt(i32::from(game_mode)).write(&mut server_data)?;
    server_data.extend_from_slice(&[0, u8::from(hardcore), 1, u8::from(accepts_self_signed)]);
    push_string(&mut server_data, &format!("{advertisement_id:016x}"))?;
    VarInt(4).write(&mut server_data)?;

    let application_data = hex::encode(server_data);
    let application_length = u32::try_from(application_data.len())
        .map_err(|_| Error::new(ErrorKind::InvalidInput, "NetherNet MOTD is too long"))?;
    let mut body = Vec::with_capacity(size_of::<u32>() + application_data.len());
    body.extend_from_slice(&application_length.to_le_bytes());
    body.extend_from_slice(application_data.as_bytes());
    encode_packet(RESPONSE_PACKET, network_id, &body, body.len())
}

pub(super) fn encode_message(
    network_id: u64,
    recipient_id: u64,
    data: &str,
) -> Result<Vec<u8>, Error> {
    let length = data.len().min(MAX_DECLARED_MESSAGE_LENGTH);
    let mut body = Vec::with_capacity(MESSAGE_HEADER_SIZE + data.len());
    body.extend_from_slice(&recipient_id.to_le_bytes());
    body.extend_from_slice(&(length as u32).to_le_bytes());
    body.extend_from_slice(data.as_bytes());
    encode_packet(
        MESSAGE_PACKET,
        network_id,
        &body,
        MESSAGE_HEADER_SIZE + length,
    )
}

fn encode_packet(
    packet_id: u16,
    network_id: u64,
    body: &[u8],
    declared_body_length: usize,
) -> Result<Vec<u8>, Error> {
    let actual_length = size_of::<u16>() + HEADER_SIZE + body.len();
    if actual_length > usize::from(u16::MAX) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "NetherNet packet is too long",
        ));
    }
    let declared_length = (size_of::<u16>() + HEADER_SIZE + declared_body_length) as u16;
    let mut payload = Vec::with_capacity(actual_length + 16);
    payload.extend_from_slice(&declared_length.to_le_bytes());
    payload.extend_from_slice(&packet_id.to_le_bytes());
    payload.extend_from_slice(&network_id.to_le_bytes());
    payload.extend_from_slice(&[0; 8]);
    payload.extend_from_slice(body);

    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(KEY.as_slice())
        .map_err(|_| Error::other("invalid NetherNet discovery key"))?;
    mac.update(&payload);
    let checksum = mac.finalize().into_bytes();

    let padding = 16 - payload.len() % 16;
    payload.resize(payload.len() + padding, padding as u8);
    encrypt(&mut payload);

    let mut response = Vec::with_capacity(CHECKSUM_SIZE + payload.len());
    response.extend_from_slice(&checksum);
    response.extend_from_slice(&payload);
    Ok(response)
}

fn push_string(buffer: &mut Vec<u8>, value: &str) -> Result<(), Error> {
    let length = u32::try_from(value.len())
        .map_err(|_| Error::new(ErrorKind::InvalidInput, "NetherNet MOTD field is too long"))?;
    VarUInt(length).write(buffer)?;
    buffer.extend_from_slice(value.as_bytes());
    Ok(())
}

fn encrypt(data: &mut [u8]) {
    let (blocks, remainder) = Block::slice_as_chunks_mut(data);
    debug_assert!(remainder.is_empty());
    Aes256::new((&*KEY).into()).encrypt_blocks(blocks);
}

fn decrypt(data: &mut [u8]) -> Option<()> {
    let (blocks, remainder) = Block::slice_as_chunks_mut(data);
    if !remainder.is_empty() {
        return None;
    }
    Aes256::new((&*KEY).into()).decrypt_blocks(blocks);
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_multibyte_string_lengths() {
        for (value, prefix) in [
            ("a".repeat(127), vec![127]),
            ("é".repeat(64), vec![128, 1]),
            ("a".repeat(255), vec![255, 1]),
            ("é".repeat(128), vec![128, 2]),
        ] {
            let mut encoded = Vec::new();
            push_string(&mut encoded, &value).unwrap();
            assert_eq!(&encoded[..prefix.len()], prefix);
            assert_eq!(&encoded[prefix.len()..], value.as_bytes());
        }
    }

    #[test]
    fn decodes_reference_discovery_request() {
        let request = hex::decode(
            "b3eca3eb83a6fcb079faf2eae2bf8abbaadb5906bc42bd63a0056274a26e013f\
             6b9027ddcd144fe3ada066f65b76e8faff0f8709e13bf6858e2051c321db615e",
        )
        .unwrap();
        assert_eq!(decode_packet(&request), Some(Packet::Request));
    }

    #[test]
    fn decodes_current_discovery_request() {
        let request = hex::decode(
            "ba539dab685df1c59ba406ec900f48e8249d28821c6e7149309d3fc289002201\
             ee5a9a7b4fec49402ac54510cb744a96ff0f8709e13bf6858e2051c321db615e",
        )
        .unwrap();
        assert_eq!(decode_packet(&request), Some(Packet::Request));
    }

    #[test]
    fn encodes_current_server_data() {
        for (accepts_self_signed, authentication_flags) in [(false, "0100"), (true, "0101")] {
            let response = encode_response(
                99,
                0x9bb64bcf14727bdb,
                "Dedicated Server",
                "Creative level",
                1,
                0,
                10,
                false,
                accepts_self_signed,
            )
            .unwrap();
            let mut payload = response[CHECKSUM_SIZE..].to_vec();
            decrypt(&mut payload).unwrap();
            let padding = usize::from(*payload.last().unwrap());
            payload.truncate(payload.len() - padding);
            assert_eq!(
                usize::from(u16::from_le_bytes(payload[..2].try_into().unwrap())),
                payload.len()
            );
            let application_length =
                u32::from_le_bytes(payload[20..24].try_into().unwrap()) as usize;
            let server_data = hex::decode(&payload[24..24 + application_length]).unwrap();
            assert_eq!(
                hex::encode(server_data),
                format!(
                    "071044656469636174656420536572766572e22207312e32362e3633\
             0e4372656174697665206c6576656c0014020000{authentication_flags}103962623634626366313437323762646208"
                )
            );
        }
    }

    #[test]
    fn message_packet_roundtrip() {
        let message = encode_message(99, 42, "CONNECTREQUEST 7 offer").unwrap();
        assert_eq!(
            decode_packet(&message),
            Some(Packet::Message {
                sender_id: 99,
                recipient_id: 42,
                data: "CONNECTREQUEST 7 offer".to_owned(),
            })
        );
    }

    #[test]
    fn reads_the_full_sdp_when_the_client_caps_both_udp_lengths() {
        let signal = format!("CONNECTREQUEST 32 {}", "a=ice-candidate:abc\r\n".repeat(96));
        let message = encode_message(2, 3, &signal).unwrap();
        let mut payload = message[CHECKSUM_SIZE..].to_vec();
        decrypt(&mut payload).unwrap();
        assert_eq!(u16::from_le_bytes(payload[..2].try_into().unwrap()), 1172);
        assert_eq!(
            u32::from_le_bytes(payload[28..32].try_into().unwrap()),
            1140
        );
        assert_eq!(
            decode_packet(&message),
            Some(Packet::Message {
                sender_id: 2,
                recipient_id: 3,
                data: signal,
            })
        );
    }

    #[test]
    fn ping_pong_message_packet() {
        let ping = encode_message(42, 99, "Ping").unwrap();
        assert_eq!(
            decode_packet(&ping),
            Some(Packet::Message {
                sender_id: 42,
                recipient_id: 99,
                data: "Ping".to_owned(),
            })
        );
        let pong = encode_message(99, 42, "Pong").unwrap();
        assert_eq!(
            decode_packet(&pong),
            Some(Packet::Message {
                sender_id: 99,
                recipient_id: 42,
                data: "Pong".to_owned(),
            })
        );
    }
}
