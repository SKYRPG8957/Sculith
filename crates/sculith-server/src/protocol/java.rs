//! Server-list discovery and the protocol-777 shared-world adapter.

mod auth;
mod chat;
mod transport;
pub use auth::Authentication;
mod chunk;
#[cfg(test)]
pub(super) use chunk::{encode as encode_chunk_for_tests, light_update as encode_light_for_tests};
mod codec;
mod commands;
mod forms;
mod inventory;
mod models;
mod peers;
mod registry;
mod session;
#[cfg(test)]
pub(in crate::protocol) mod test_client;

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use sculith_core::ServerStatus;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::JoinSet,
};

#[cfg(test)]
const MAX_PACKET_BYTES: usize = codec::MAX_INBOUND;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: usize = 64;

#[derive(Clone)]
pub struct Preview {
    pub authentication: Option<Arc<Authentication>>,
    pub content: Option<Arc<crate::content::Content>>,
    pub world: crate::world::WorldHandle,
    pub timeout: Duration,
    pub shutdown: watch::Receiver<bool>,
}

#[derive(Clone, Copy)]
enum Phase {
    Login,
    Configuration,
    Play,
}

pub async fn serve(
    listener: TcpListener,
    status: watch::Receiver<ServerStatus>,
    preview: Option<Preview>,
    mut shutdown: watch::Receiver<bool>,
    observer: crate::connections::Observer,
) -> Result<()> {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            changed=shutdown.changed()=>{
                if changed.is_err() || *shutdown.borrow() { break; }
            }
            accepted = listener.accept() => {
                let (mut stream, address) = accepted?;
                let Ok(permit) = slots.clone().try_acquire_owned() else { continue };
                let status = status.clone();
                let preview=preview.clone();
                let observer=observer.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    if let Err(error)=handle_with_preview(&mut stream,status,preview,&observer).await {
                        tracing::debug!(%address,error = %format!("{error:#}"),"Java connection closed");
                    }
                });
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                result.context("Java connection task failed")?;
            }
        }
    }
    connections.abort_all();
    while let Some(result) = connections.join_next().await {
        if let Err(error) = result {
            ensure!(error.is_cancelled(), "Java child task panicked");
        }
    }
    Ok(())
}

#[cfg(test)]
async fn handle(stream: &mut TcpStream, status: watch::Receiver<ServerStatus>) -> Result<()> {
    handle_with_preview(
        stream,
        status,
        None,
        &crate::connections::Observer::default(),
    )
    .await
}

async fn handle_with_preview(
    stream: &mut TcpStream,
    status: watch::Receiver<ServerStatus>,
    preview: Option<Preview>,
    observer: &crate::connections::Observer,
) -> Result<()> {
    use sculith_core::connections::PacketPhase;
    let peer = stream.peer_addr()?;
    let mut frames = codec::Frames::observed(observer);
    let mut secured = transport::Stream::observed(stream, frames.trace());
    let stream = &mut secured;
    let handshake = tokio::time::timeout(CONNECTION_TIMEOUT, frames.next(stream)).await??;
    let (protocol, state) = parse_handshake(&handshake)?;
    frames.select_protocol(protocol as u32);
    frames.set_phase(if state == 2 {
        PacketPhase::Login
    } else {
        PacketPhase::Status
    });
    if state == 2 {
        if let Some(preview) = preview.as_ref().filter(|_| protocol == registry::PROTOCOL) {
            ensure!(
                preview.authentication.is_some() || peer.ip().is_loopback(),
                "Java offline sessions require a loopback peer"
            );
            let max_players = status.borrow().max_players;
            let mut phase = Phase::Login;
            let started = tokio::time::timeout(
                preview.timeout,
                session::start(
                    stream,
                    &mut frames,
                    preview.world.clone(),
                    max_players,
                    &mut phase,
                    preview.content.as_deref(),
                    preview.authentication.as_deref(),
                ),
            )
            .await;
            let result = match started {
                Ok(Ok(session)) => {
                    session
                        .run(stream, &mut frames, preview.shutdown.clone())
                        .await
                }
                Ok(Err(error)) => Err(error),
                Err(error) => Err(error).context("Java startup timed out"),
            };
            if result.is_err() {
                let _ = disconnect(
                    stream,
                    phase,
                    "Sculith: request rejected; check the server log.",
                )
                .await;
            }
            return result;
        }
        // Consume Login Start before closing so queued client data does not cause
        // a TCP reset that hides the explanatory disconnect from vanilla clients.
        let login_start = tokio::time::timeout(CONNECTION_TIMEOUT, frames.next(stream)).await??;
        let mut fields = &login_start[..];
        ensure!(read_varint(&mut fields)? == 0, "expected login start");
        let reason = if preview.is_some() {
            "This server requires Java 26.3."
        } else {
            "Java connections are disabled in the server configuration."
        };
        disconnect(stream, Phase::Login, reason).await?;
        return Ok(());
    }

    tokio::time::timeout(
        CONNECTION_TIMEOUT,
        status_exchange(stream, &mut frames, status, preview.is_some()),
    )
    .await?
}

async fn status_exchange(
    stream: &mut transport::Stream<&mut TcpStream>,
    frames: &mut codec::Frames,
    status: watch::Receiver<ServerStatus>,
    preview: bool,
) -> Result<()> {
    let request = frames.next(stream).await?;
    ensure!(request == [0], "expected status request");
    let snapshot = status.borrow().clone();
    let reference = super::references().java;
    let response = serde_json::json!({
        "version": {"name": if preview {"Sculith / Java 26.3"} else {"Sculith / status only"},
            "protocol": if preview {registry::PROTOCOL} else {-1}},
        "players": {"max": snapshot.max_players, "online": snapshot.online_players, "sample": []},
        "description": {"text": snapshot.motd},
        "sculith": {"stage": snapshot.stage, "reference_version": reference.version, "reference_protocol": reference.protocol}
    });
    let mut packet = vec![0];
    write_string(&response.to_string(), &mut packet);
    codec::send(stream, &packet).await?;
    // The optional ping is framed independently; the payload must be echoed unchanged.
    let ping = frames.next(stream).await?;
    ensure!(
        ping.len() == 9 && ping[0] == 1,
        "expected status ping with an i64 payload"
    );
    codec::send(stream, &ping).await?;
    Ok(())
}

fn parse_handshake(mut packet: &[u8]) -> Result<(i32, i32)> {
    ensure!(read_varint(&mut packet)? == 0, "expected handshake packet");
    let protocol = read_varint(&mut packet)?;
    ensure!(protocol >= 0, "negative client protocol");
    let length = read_varint(&mut packet)?;
    ensure!(
        (1..=1024).contains(&length),
        "invalid server address length"
    );
    let length = length as usize;
    ensure!(packet.len() >= length + 2, "truncated handshake address");
    std::str::from_utf8(&packet[..length]).context("invalid UTF-8 server address")?;
    packet = &packet[length + 2..];
    let state = read_varint(&mut packet)?;
    ensure!(packet.is_empty(), "trailing handshake bytes");
    ensure!(state == 1 || state == 2, "unsupported handshake state");
    Ok((protocol, state))
}

async fn disconnect(
    stream: &mut transport::Stream<impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>,
    phase: Phase,
    reason: &str,
) -> Result<()> {
    let mut packet = codec::packet(match phase {
        Phase::Login => 0,
        Phase::Configuration => 2,
        Phase::Play => 0x20,
    });
    match phase {
        Phase::Login => write_string(&serde_json::json!({"text":reason}).to_string(), &mut packet),
        _ => codec::text(reason, &mut packet)?,
    }
    codec::send(stream, &packet).await?;
    // Dropping a TCP socket with unread client data can reset it and discard
    // the rejection notice on Windows. Half-close first; drain without parsing
    // further requests, bounded by both bytes and time.
    stream.shutdown().await?;
    let _ = tokio::time::timeout(Duration::from_millis(200), async {
        let mut remaining = codec::MAX_INBOUND;
        let mut buffer = [0; 1024];
        while remaining != 0 {
            let length = remaining.min(buffer.len());
            let read = stream.read(&mut buffer[..length]).await?;
            if read == 0 {
                break;
            }
            remaining -= read;
        }
        Ok::<_, std::io::Error>(())
    })
    .await;
    Ok(())
}

pub fn read_varint(input: &mut &[u8]) -> Result<i32> {
    let mut value = 0u32;
    for index in 0..5 {
        let byte = *input.first().context("truncated VarInt")?;
        *input = &input[1..];
        ensure!(index != 4 || byte & 0xf0 == 0, "VarInt overflows 32 bits");
        value |= u32::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Ok(value as i32);
        }
    }
    bail!("VarInt exceeds five bytes")
}

pub fn write_varint(value: i32, output: &mut Vec<u8>) {
    let mut value = value as u32;
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn write_string(value: &str, output: &mut Vec<u8>) {
    write_varint(value.len() as i32, output);
    output.extend_from_slice(value.as_bytes());
}

#[cfg(test)]
async fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>> {
    codec::Frames::default().next(stream).await
}

#[cfg(test)]
async fn write_frame(stream: &mut TcpStream, payload: &[u8]) -> Result<()> {
    codec::send(&mut transport::Stream::new(stream), payload).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_cover_signed_values_boundaries_and_overflow() {
        for number in [0, 1, 127, 128, 255, 777, i32::MAX, -1, i32::MIN] {
            let mut bytes = Vec::new();
            write_varint(number, &mut bytes);
            let mut input = &bytes[..];
            assert_eq!(read_varint(&mut input).unwrap(), number);
            assert!(input.is_empty());
        }
        for invalid in [
            &[0x80][..],
            &[0xff, 0xff, 0xff, 0xff, 0x10],
            &[0x80, 0x80, 0x80, 0x80, 0x80, 0],
        ] {
            assert!(read_varint(&mut &invalid[..]).is_err());
        }
    }

    #[test]
    fn handshake_rejects_truncation_and_trailing_bytes() {
        let mut packet = vec![0];
        write_varint(777, &mut packet);
        write_string("localhost", &mut packet);
        packet.extend_from_slice(&25565u16.to_be_bytes());
        packet.push(1);
        assert_eq!(parse_handshake(&packet).unwrap(), (777, 1));
        for length in 0..packet.len() {
            assert!(parse_handshake(&packet[..length]).is_err());
        }
        packet.push(0);
        assert!(parse_handshake(&packet).is_err());
    }

    #[tokio::test]
    async fn status_and_ping_cross_real_tcp_with_fragmented_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (_, status) = watch::channel(ServerStatus::new("공통 서버".into(), 32).unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            handle(&mut stream, status).await.unwrap();
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        let mut handshake = vec![0];
        write_varint(777, &mut handshake);
        write_string("localhost", &mut handshake);
        handshake.extend_from_slice(&address.port().to_be_bytes());
        handshake.push(1);
        let mut bytes = Vec::new();
        write_varint(handshake.len() as i32, &mut bytes);
        bytes.extend_from_slice(&handshake);
        for byte in bytes {
            client.write_all(&[byte]).await.unwrap();
        }
        write_frame(&mut client, &[0]).await.unwrap();
        let reply = read_frame(&mut client).await.unwrap();
        assert_eq!(reply[0], 0);
        let mut data = &reply[1..];
        let length = read_varint(&mut data).unwrap();
        assert_eq!(data.len(), length as usize);
        let status: serde_json::Value = serde_json::from_slice(data).unwrap();
        assert_eq!(status["players"]["max"], 32);
        assert_eq!(status["version"]["protocol"], -1);
        assert!(
            status["description"]["text"]
                .as_str()
                .unwrap()
                .contains("공통 서버")
        );
        let mut ping = vec![1];
        ping.extend_from_slice(&(-123_456i64).to_be_bytes());
        write_frame(&mut client, &ping).await.unwrap();
        assert_eq!(read_frame(&mut client).await.unwrap(), ping);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn disconnect_reaches_client_before_eof_with_unread_pipelined_input() {
        use tokio::io::AsyncReadExt;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (ready, queued) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            queued.await.unwrap();
            disconnect(
                &mut transport::Stream::new(&mut stream),
                Phase::Play,
                "request rejected",
            )
            .await
            .unwrap();
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(&[0; 2048]).await.unwrap();
        ready.send(()).unwrap();
        let packet = read_frame(&mut client).await.unwrap();
        assert_eq!(packet[0], 0x20);
        assert!(packet.windows(16).any(|w| w == b"request rejected"));
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), client.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        drop(client);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn disabled_java_login_sends_configuration_reason_and_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (_, status) = watch::channel(ServerStatus::new("test".into(), 20).unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            handle(&mut stream, status).await.unwrap();
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        let mut handshake = vec![0];
        write_varint(777, &mut handshake);
        write_string("localhost", &mut handshake);
        handshake.extend_from_slice(&address.port().to_be_bytes());
        handshake.push(2);
        write_frame(&mut client, &handshake).await.unwrap();
        let mut login_start = vec![0];
        write_string("SculithTest", &mut login_start);
        login_start.extend_from_slice(&[0; 16]);
        write_frame(&mut client, &login_start).await.unwrap();
        let disconnect = read_frame(&mut client).await.unwrap();
        assert_eq!(disconnect[0], 0);
        let mut json = &disconnect[1..];
        let length = read_varint(&mut json).unwrap();
        assert_eq!(length as usize, json.len());
        let reason: serde_json::Value = serde_json::from_slice(json).unwrap();
        assert!(
            reason["text"]
                .as_str()
                .unwrap()
                .contains("Java connections are disabled")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected_before_allocating_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(read_frame(&mut stream).await.is_err());
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        let mut bytes = vec![];
        write_varint(MAX_PACKET_BYTES as i32 + 1, &mut bytes);
        client.write_all(&bytes).await.unwrap();
        server.await.unwrap();
    }
}
