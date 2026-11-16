use super::*;

pub(crate) struct TestIssuer {
    key: aws_lc_rs::signature::RsaKeyPair,
    kid: &'static str,
}

impl TestIssuer {
    pub(crate) fn new(kid: &'static str) -> Self {
        Self {
            key: aws_lc_rs::signature::RsaKeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048)
                .unwrap(),
            kid,
        }
    }

    pub(crate) fn keys(&self) -> nethernet::identity::jwk::JwkSet {
        use aws_lc_rs::signature::KeyPair as _;
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        let public = aws_lc_rs::rsa::PublicKeyComponents::<Vec<u8>>::from(self.key.public_key());
        nethernet::identity::jwk::JwkSet {
            keys: vec![nethernet::identity::jwk::Jwk {
                kty: "RSA".into(),
                kid: Some(self.kid.into()),
                n: Some(URL_SAFE_NO_PAD.encode(public.n)),
                e: Some(URL_SAFE_NO_PAD.encode(public.e)),
            }],
        }
    }

    pub(crate) fn offer(&self, signed: &str, patch: serde_json::Value) -> String {
        let old = signed
            .lines()
            .find_map(|line| line.strip_prefix("a=identity:"))
            .unwrap();
        let mut identity = nethernet::identity::Identity::from_base64(old).unwrap();
        identity.assertion.token = self.token(&identity.assertion.token, patch);
        signed.replace(old, &identity.to_base64().unwrap())
    }

    pub(crate) fn token(&self, original: &str, patch: serde_json::Value) -> String {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        let encoded = original.split('.').nth(1).unwrap();
        let mut claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).unwrap()).unwrap();
        claims["iss"] = nethernet::identity::MINECRAFT_ISSUER.into();
        claims["aud"] = nethernet::identity::MINECRAFT_AUDIENCE.into();
        claims["sub"] = "2535400000000001".into();
        for (field, value) in patch.as_object().unwrap() {
            claims[field] = value.clone();
        }
        let header = serde_json::json!({"alg":"RS256","kid":self.kid});
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        let mut signature = vec![0; self.key.public_modulus_len()];
        self.key
            .sign(
                &aws_lc_rs::signature::RSA_PKCS1_SHA256,
                &aws_lc_rs::rand::SystemRandom::new(),
                input.as_bytes(),
                &mut signature,
            )
            .unwrap();
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    pub(crate) fn oidc(&self) -> bedrock::auth::AuthOIDC {
        let keys = serde_json::from_slice(&serde_json::to_vec(&self.keys()).unwrap()).unwrap();
        let mut oidc = bedrock::auth::AuthOIDC::new(
            nethernet::identity::MINECRAFT_ISSUER,
            nethernet::identity::MINECRAFT_AUDIENCE,
            nethernet::identity::MINECRAFT_KEYS_URL,
            keys,
        );
        oidc.refresh_interval = Duration::MAX;
        oidc
    }
}

pub(in crate::protocol::bedrock) async fn server(
    address: SocketAddr,
    path: &Path,
    media: Option<&PublicMedia>,
    issuer: &TestIssuer,
) -> Result<(Server, Arc<ServerIdentity>)> {
    let listener = TcpListener::bind(address).await?;
    let server = prepare_bound(
        listener,
        path,
        media,
        Some(keys::Authority::from_keys(issuer.keys())),
    )
    .await?;
    let operator = server.identity.clone();
    Ok((server, operator))
}

/// Uses real HTTP/ICE/DTLS/SCTP with a test-issued account token. No protocol or
/// account checks are disabled in the server; only its trusted issuer is a fixture.
pub(in crate::protocol::bedrock) async fn connect(
    address: SocketAddr,
    socket: UdpSocket,
    signer: &ServerIdentity,
    operator: &ServerIdentity,
    issuer: &TestIssuer,
    shutdown: watch::Receiver<bool>,
    extra_candidate: Option<SocketAddr>,
) -> Result<(SessionTransport, tokio::task::JoinHandle<Result<()>>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let local = socket.local_addr()?;
    let bind_ip = local.ip();
    let now = Instant::now();
    let (session, description) = Session::new(local, true, now)?;
    let network_id = format!("http://{address}");
    let (mut peer, signals) =
        Peer::connect(session, description, 17, network_id.clone(), IceMode::Full);
    let mut signed = signer.augment(&signals[0].data, SystemTime::now())?;
    if let Some(extra) = extra_candidate {
        signed.push_str(&format!(
            "a=candidate:987654 1 UDP 2130706431 {} {} typ host\r\n",
            extra.ip(),
            extra.port()
        ));
    }
    let sdp = issuer.offer(&signed, serde_json::json!({}));
    let claims = nethernet::identity::validate_sdp(
        &sdp,
        &TokenTrust::Minecraft(issuer.keys().into()),
        SystemTime::now(),
    )?;
    let identity = Arc::new(nethernet::identity::PlayerInfo::new(
        claims,
        "authenticated-fixture".into(),
        Some(address),
    ));
    let mut http = tokio::net::TcpStream::connect(address).await?;
    let request = format!(
        "POST /v1/join/authenticated-fixture HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sdp}",
        sdp.len()
    );
    http.write_all(request.as_bytes()).await?;
    let mut bytes = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        http.take(HTTP_BODY_BYTES as u64 + 8192)
            .read_to_end(&mut bytes),
    )
    .await??;
    let response = String::from_utf8(bytes)?;
    ensure!(
        response.starts_with("HTTP/1.1 200"),
        "fixture offer rejected: {response}"
    );
    let (_, sdp) = response
        .split_once("\r\n\r\n")
        .context("HTTP answer has no body")?;
    let claims = nethernet::identity::validate_sdp(sdp, &TokenTrust::Any, SystemTime::now())?;
    ensure!(
        &claims.client_public_key()? == operator.verifying_key(),
        "answer changed the pinned operator key"
    );
    peer.handle(ConnectionInput::Signal(
        nethernet::protocol::Signal::answer(17, sdp.into(), network_id),
        Instant::now(),
    ))?;
    let (ready, mut received) = mpsc::channel(1);
    let config = PublicMedia {
        ports: [local.port(), local.port()],
        advertised_ip: Some(bind_ip),
    };
    let media = Media::prepare(bind_ip, Some(&config))?.for_peer(Some(address))?;
    let task = tokio::spawn(drive(
        socket,
        peer,
        PeerContext {
            shutdown,
            ready,
            media,
            identity,
            incoming_budget: Arc::new(Semaphore::new(QUEUED_BYTES)),
            outgoing_budget: Arc::new(Semaphore::new(QUEUED_BYTES)),
        },
    ));
    let transport = tokio::time::timeout(Duration::from_secs(10), received.recv())
        .await?
        .context("fixture media driver stopped before ready")?;
    Ok((transport, task))
}
