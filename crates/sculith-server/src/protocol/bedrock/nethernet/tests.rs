use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn operator_key_is_stable_and_existing_foreign_files_are_preserved() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("identity.pem");
    let original = load_identity(&path).unwrap();
    let persisted = fs::read(&path).unwrap();
    let restored = load_identity(&path).unwrap();
    assert_eq!(original.verifying_key(), restored.verifying_key());
    assert_eq!(fs::read(&path).unwrap(), persisted);
    fs::write(&path, b"existing foreign user data").unwrap();
    assert!(load_identity(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"existing foreign user data");
}

#[cfg(unix)]
#[test]
fn operator_key_is_private_and_symlink_destinations_are_refused() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("identity.pem");
    load_identity(&path).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let alias = directory.path().join("alias.pem");
    symlink(&path, &alias).unwrap();
    let original = fs::read(&path).unwrap();
    assert!(load_identity(&alias).is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
}

async fn exchange(address: SocketAddr, request: &[u8]) -> String {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream.write_all(request).await.unwrap();
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_string(&mut response))
        .await
        .unwrap()
        .unwrap();
    response
}

#[tokio::test]
async fn loopback_media_only_checks_loopback_candidates_on_advertised_ports() {
    let (_session, description) =
        Session::new("127.0.0.1:54321".parse().unwrap(), true, Instant::now()).unwrap();
    let sdp = description.encode_full(&[])
        + "a=candidate:1 1 UDP 2130706431 192.0.2.10 54321 typ host generation 0\r\n";
    let identity = ServerIdentity::generate("test.operator", SystemTime::now()).unwrap();
    let offer = Offer {
        connection_id: 1,
        network_id: "test.client".into(),
        sdp: identity.augment(&sdp, SystemTime::now()).unwrap(),
        client_address: Some("127.0.0.1:12345".parse().unwrap()),
        host: None,
        player: None,
    };
    let (_socket, mut peer, _answer) = answer(
        &offer,
        "127.0.0.1".parse().unwrap(),
        &identity,
        Duration::from_secs(10),
        &LocalMedia::from_addresses("127.0.0.1".parse().unwrap(), Vec::new()),
    )
    .await
    .unwrap();
    let now = Instant::now();
    let mut checked = 0;
    for offset in 0..10 {
        peer.handle(ConnectionInput::Timeout(
            now + Duration::from_millis(offset * 100),
        ))
        .unwrap();
        while let Some(output) = peer.poll() {
            if let SessionOutput::Send(_, remote) = output {
                assert!(remote.ip().is_loopback(), "loopback socket tried {remote}");
                assert_eq!(remote.port(), 54321);
                checked += 1;
            }
        }
    }
    assert!(checked > 0, "no loopback connectivity check was attempted");
}

#[tokio::test]
async fn http_status_and_rejections_cross_tcp_without_admitting_invalid_offers() {
    let directory = tempfile::tempdir().unwrap();
    let server = prepare(
        "127.0.0.1:0".parse().unwrap(),
        &directory.path().join("identity.pem"),
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    let (stop, shutdown) = watch::channel(false);
    let (world, owner) = crate::world::create(shutdown.clone());
    let owner = tokio::spawn(owner);
    let (status_tx, status) =
        watch::channel(ServerStatus::new("shared NetherNet status".into(), 20).unwrap());
    let task = tokio::spawn(server.serve(ContextData {
        authentication: Authentication::offline(),
        timeout: Duration::from_secs(10),
        world: world.clone(),
        status,
        shutdown,
    }));
    let response = exchange(
        address,
        b"GET /v1/join HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let body: serde_json::Value =
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["name"], "shared NetherNet status");
    assert_eq!(body["protocol"], 2225);
    assert_eq!(body["gameType"], 1);
    assert_eq!(body["players"], 0);
    let missing = exchange(
        address,
        b"POST /v1/join/test HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
    )
    .await;
    assert!(missing.starts_with("HTTP/1.1 401"), "{missing}");

    let identity = ServerIdentity::generate("test.client", SystemTime::now()).unwrap();
    let sdp = identity.augment("v=0\r\na=fingerprint:sha-256 AB:CD\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n", SystemTime::now()).unwrap();
    let tampered = sdp.replace("AB:CD", "EF:01");
    let request = format!(
        "POST /v1/join/test HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{tampered}",
        tampered.len()
    );
    let invalid = exchange(address, request.as_bytes()).await;
    assert!(invalid.starts_with("HTTP/1.1 401"), "{invalid}");

    // Request an actual answer and independently check its persistent operator
    // key and fingerprint signature, including a peer advertising no SCTP cap.
    let (_session, mut description) =
        Session::new("127.0.0.1:12345".parse().unwrap(), true, Instant::now()).unwrap();
    description.sctp_max_message_size = 0;
    let sdp = description.encode_full(&[]);
    let sdp = identity.augment(&sdp, SystemTime::now()).unwrap();
    let request = format!(
        "POST /v1/join/test HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    );
    let accepted = exchange(address, request.as_bytes()).await;
    assert!(
        accepted.starts_with("HTTP/1.1 200"),
        "expected signed answer"
    );
    let answer = accepted.split_once("\r\n\r\n").unwrap().1;
    let claims =
        nethernet::identity::validate_sdp(answer, &TokenTrust::Any, SystemTime::now()).unwrap();
    assert_eq!(
        &claims.client_public_key().unwrap(),
        load_identity(&directory.path().join("identity.pem"))
            .unwrap()
            .verifying_key()
    );
    let tampered = answer.replace("a=fingerprint:sha-256 ", "a=fingerprint:sha-256 00:");
    assert!(
        nethernet::identity::validate_sdp(&tampered, &TokenTrust::Any, SystemTime::now()).is_err()
    );

    let oversized = exchange(
        address,
        b"POST /v1/join/test HTTP/1.1\r\nHost: localhost\r\nContent-Length: 65537\r\n\r\n",
    )
    .await;
    assert!(oversized.starts_with("HTTP/1.1 413"), "{oversized}");
    // Chunked transfer must not bypass the body limit either.
    let body = "x".repeat(HTTP_BODY_BYTES + 1);
    let request = format!(
        "POST /v1/join/test HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
        body.len()
    );
    let oversized = exchange(address, request.as_bytes()).await;
    assert!(oversized.starts_with("HTTP/1.1 413"), "{oversized}");
    assert_eq!(world.online_players(), 0);
    // Invalid attempts neither crash the common service nor change its status.
    let response = exchange(address, b"GET /v1/join HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    stop.send_replace(true);
    task.await.unwrap().unwrap();
    owner.await.unwrap().unwrap();
    drop(status_tx);
    let _rebound = TcpListener::bind(address).await.unwrap();
}
