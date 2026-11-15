//! Modern Java server-list status and a deliberate login rejection.
//! The -1 advertised protocol prevents claiming gameplay compatibility.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use sculith_core::ServerStatus;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::JoinSet,
};

const MAX_PACKET_BYTES: usize = 8192;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: usize = 64;

pub async fn serve(listener: TcpListener, status: watch::Receiver<ServerStatus>) -> Result<()> {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (mut stream, address) = accepted?;
                let Ok(permit) = slots.clone().try_acquire_owned() else { continue };
                let status = status.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    match tokio::time::timeout(CONNECTION_TIMEOUT, handle(&mut stream, status)).await {
                        Ok(Ok(())) => {},
                        Ok(Err(error)) => tracing::debug!(%address, %error, "Java connection closed"),
                        Err(_) => tracing::debug!(%address, "Java connection timed out"),
                    }
                });
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                result.context("Java connection task failed")?;
            }
        }
    }
}

async fn handle(stream: &mut TcpStream, status: watch::Receiver<ServerStatus>) -> Result<()> {
    let handshake = read_frame(stream).await?;
    let state = parse_handshake(&handshake)?;
    if state == 2 {
        // Consume Login Start before closing so queued client data does not cause
        // a TCP reset that hides the explanatory disconnect from vanilla clients.
        let login_start = read_frame(stream).await?;
        let mut fields = &login_start[..];
        ensure!(read_varint(&mut fields)? == 0, "expected login start");
        let reason = serde_json::json!({"text": "Sculith development build: world login is not implemented yet."});
        let mut disconnect = vec![0];
        write_string(&reason.to_string(), &mut disconnect);
        write_frame(stream, &disconnect).await?;
        stream.shutdown().await?;
        return Ok(());
    }

    let request = read_frame(stream).await?;
    ensure!(request == [0], "expected status request");
    let snapshot = status.borrow().clone();
    let reference = super::references().java;
    let response = serde_json::json!({
        "version": {"name": "Sculith / discovery only", "protocol": -1},
        "players": {"max": snapshot.max_players, "online": snapshot.online_players, "sample": []},
        "description": {"text": format!("{}\nDevelopment build: world login unavailable", snapshot.motd)},
        "sculith": {"stage": snapshot.stage, "reference_version": reference.version, "reference_protocol": reference.protocol}
    });
    let mut packet = vec![0];
    write_string(&response.to_string(), &mut packet);
    write_frame(stream, &packet).await?;
    // The optional ping is framed independently; the payload must be echoed unchanged.
    let ping = read_frame(stream).await?;
    ensure!(
        ping.len() == 9 && ping[0] == 1,
        "expected status ping with an i64 payload"
    );
    write_frame(stream, &ping).await?;
    Ok(())
}

fn parse_handshake(mut packet: &[u8]) -> Result<i32> {
    ensure!(read_varint(&mut packet)? == 0, "expected handshake packet");
    ensure!(read_varint(&mut packet)? >= 0, "negative client protocol");
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
    Ok(state)
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

async fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut prefix = Vec::with_capacity(5);
    loop {
        let byte = stream.read_u8().await?;
        prefix.push(byte);
        ensure!(prefix.len() <= 5, "frame length exceeds five bytes");
        if byte & 0x80 == 0 {
            break;
        }
        ensure!(prefix.len() < 5, "frame length exceeds five bytes");
    }
    let length = read_varint(&mut &prefix[..])?;
    ensure!(
        (1..=MAX_PACKET_BYTES as i32).contains(&length),
        "frame length out of bounds"
    );
    let mut frame = vec![0; length as usize];
    stream.read_exact(&mut frame).await?;
    Ok(frame)
}

async fn write_frame(stream: &mut TcpStream, payload: &[u8]) -> Result<()> {
    let mut frame = Vec::new();
    write_varint(payload.len() as i32, &mut frame);
    frame.extend_from_slice(payload);
    stream.write_all(&frame).await?;
    Ok(())
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
        assert_eq!(parse_handshake(&packet).unwrap(), 1);
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
    async fn login_start_receives_an_explicit_development_disconnect() {
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
                .contains("world login is not implemented")
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
