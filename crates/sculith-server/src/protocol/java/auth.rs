//! Stock Java key exchange and bounded official session verification.
use super::{
    codec::{self, Fields, Frames},
    transport::Stream,
    write_string, write_varint,
};
use anyhow::{Context, Result, ensure};
use aws_lc_rs::{
    encoding::AsDer,
    rand,
    rsa::{KeySize, Pkcs1PrivateDecryptingKey, PrivateDecryptingKey},
};
use serde::Deserialize;
use sha1::{Digest, Sha1};
use std::{io::Read, sync::Arc};
use tokio::net::TcpStream;
use uuid::Uuid;
use zeroize::Zeroizing;

const SESSION_ENDPOINT: &str = "https://sessionserver.mojang.com/session/minecraft/hasJoined";
const MAX_PROFILE: u64 = 65_536;

#[derive(Clone)]
pub struct Authentication {
    key: Arc<Pkcs1PrivateDecryptingKey>,
    public_key: Arc<Vec<u8>>,
    http: ureq::Agent,
    workers: Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    verifier: Option<Arc<TestVerifier>>,
}
#[cfg(test)]
type TestVerifier = dyn Fn(&str, &str, Uuid) -> Result<VerifiedProfile> + Send + Sync;

pub struct VerifiedProfile {
    pub name: String,
    pub uuid: Uuid,
}

#[derive(Deserialize)]
struct SessionProfile {
    id: String,
    name: String,
}

impl SessionProfile {
    fn verify(self, requested_name: &str, claimed_uuid: Uuid) -> Result<VerifiedProfile> {
        ensure!(
            self.name.eq_ignore_ascii_case(requested_name)
                && !self.name.is_empty()
                && self.name.len() <= 16
                && self
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "session profile name mismatch"
        );
        ensure!(
            self.id.len() == 32 && self.id.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid session profile identifier"
        );
        let uuid = Uuid::parse_str(&self.id)?;
        ensure!(
            !uuid.is_nil() && uuid == claimed_uuid,
            "session profile identifier mismatch"
        );
        Ok(VerifiedProfile {
            name: self.name,
            uuid,
        })
    }
}

impl Authentication {
    pub async fn new() -> Result<Self> {
        tokio::task::spawn_blocking(|| {
            // Java accepts the DER key's actual modulus size. No private key is
            // persisted or sent to a worker/plugin.
            let private = PrivateDecryptingKey::generate(KeySize::Rsa2048)?;
            let public = private.public_key().as_der()?.as_ref().to_vec();
            Ok(Self {
                key: Arc::new(Pkcs1PrivateDecryptingKey::new(private)?),
                public_key: Arc::new(public),
                http: crate::protocol::http_client::authentication_metadata(),
                workers: Arc::new(tokio::sync::Semaphore::new(16)),
                #[cfg(test)]
                verifier: None,
            })
        })
        .await
        .context("Java key generation worker failed")?
    }

    pub async fn verify(
        &self,
        stream: &mut Stream<&mut TcpStream>,
        frames: &mut Frames,
        name: &str,
        claimed_uuid: Uuid,
    ) -> Result<VerifiedProfile> {
        let mut challenge = [0u8; 16];
        rand::fill(&mut challenge)?;
        let mut hello = codec::packet(1);
        write_string("", &mut hello);
        byte_array(&self.public_key, &mut hello);
        byte_array(&challenge, &mut hello);
        hello.push(1); // The stock client must join its official account session.
        codec::send(stream, &hello).await?;
        let packet = frames.next(stream).await?;
        let mut fields = Fields(&packet);
        ensure!(fields.int()? == 1, "expected Java Encryption Response");
        let secret = fields.byte_array(512)?.to_vec();
        let token = fields.byte_array(512)?.to_vec();
        fields.end()?;
        ensure!(
            secret.len() == self.key.key_size_bytes() && token.len() == self.key.key_size_bytes(),
            "Java key exchange rejected"
        );
        let key = self.key.clone();
        let secret = self
            .blocking(move || -> Result<Zeroizing<[u8; 16]>> {
                let mut plain_secret = Zeroizing::new(vec![0u8; key.min_output_size()]);
                let mut plain_token = Zeroizing::new(vec![0u8; key.min_output_size()]);
                // Perform both operations; expose the same error for every failed
                // padding/length/challenge check.
                let secret = key.decrypt(&secret, &mut plain_secret);
                let token = key.decrypt(&token, &mut plain_token);
                let valid = secret.as_ref().is_ok_and(|s| s.len() == 16)
                    && token.as_ref().is_ok_and(|t| {
                        aws_lc_rs::constant_time::verify_slices_are_equal(t, &challenge).is_ok()
                    });
                ensure!(valid, "Java key exchange rejected");
                Ok(Zeroizing::new(
                    secret.unwrap().try_into().expect("checked secret length"),
                ))
            })
            .await
            .context("Java key exchange worker failed")?;
        stream.enable_encryption(&secret)?;
        let hash = server_hash(&secret, &self.public_key);
        self.session_profile(name, hash, claimed_uuid).await
    }

    async fn session_profile(
        &self,
        name: &str,
        hash: String,
        claimed_uuid: Uuid,
    ) -> Result<VerifiedProfile> {
        #[cfg(test)]
        if let Some(verifier) = &self.verifier {
            return verifier(name, &hash, claimed_uuid);
        }
        let http = self.http.clone();
        let name = name.to_owned();
        self.blocking(move || {
            let mut response = http
                .get(SESSION_ENDPOINT)
                .query("username", &name)
                .query("serverId", &hash)
                .config()
                .max_redirects(0)
                .build()
                .call()
                .context("Java account session verification failed")?;
            ensure!(
                response.status().as_u16() == 200,
                "Java account session not joined"
            );
            let mut body = Vec::new();
            response
                .body_mut()
                .as_reader()
                .take(MAX_PROFILE + 1)
                .read_to_end(&mut body)?;
            ensure!(
                body.len() as u64 <= MAX_PROFILE,
                "Java session profile exceeds limit"
            );
            let profile: SessionProfile =
                serde_json::from_slice(&body).context("invalid Java session profile")?;
            profile.verify(&name, claimed_uuid)
        })
        .await
        .context("Java session verification worker failed")
    }

    async fn blocking<T: Send + 'static>(
        &self,
        operation: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        // The blocking worker retains this permit after socket cancellation.
        let permit = self
            .workers
            .clone()
            .try_acquire_owned()
            .context("Java authentication workers busy")?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            operation()
        })
        .await
        .context("Java authentication worker failed")?
    }
}

fn byte_array(value: &[u8], packet: &mut Vec<u8>) {
    write_varint(value.len() as i32, packet);
    packet.extend_from_slice(value);
}

fn server_hash(secret: &[u8; 16], public_key: &[u8]) -> String {
    let mut hash = Sha1::new();
    hash.update(secret);
    hash.update(public_key);
    signed_hex(hash.finalize().into())
}

fn signed_hex(mut bytes: [u8; 20]) -> String {
    use std::fmt::Write as _;
    let negative = bytes[0] & 128 != 0;
    if negative {
        let mut carry = 1u16;
        for byte in bytes.iter_mut().rev() {
            carry += u16::from(!*byte);
            *byte = carry as u8;
            carry >>= 8;
        }
    }
    let mut result = if negative {
        "-".to_owned()
    } else {
        String::new()
    };
    let Some(first) = bytes.iter().position(|b| *b != 0) else {
        return "0".into();
    };
    write!(result, "{:x}", bytes[first]).unwrap();
    for byte in &bytes[first + 1..] {
        write!(result, "{byte:02x}").unwrap();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_socket_work_keeps_its_worker_permit_until_the_job_ends() {
        let auth = Arc::new(Authentication::new().await.unwrap());
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let copy = auth.clone();
        let task = tokio::spawn(async move {
            copy.blocking(move || {
                started.send(()).unwrap();
                wait.recv()?;
                Ok(())
            })
            .await
        });
        ready.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(auth.workers.available_permits(), 15);
        let mut held = Vec::new();
        for _ in 0..15 {
            held.push(auth.workers.clone().try_acquire_owned().unwrap());
        }
        assert!(
            auth.blocking(|| -> Result<()> { panic!("full worker budget admitted work") })
                .await
                .is_err()
        );
        drop(held);
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while auth.workers.available_permits() != 16 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn java_hash_uses_signed_unpadded_twos_complement_sha1() {
        for (input, expected) in [
            ("Notch", "4ed1f46bbe04bc756bcb17c0c7ce3e4632f06a48"),
            ("jeb_", "-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1"),
            ("simon", "88e16a1019277b15d58faf0541e11910eb756f6"),
        ] {
            assert_eq!(signed_hex(Sha1::digest(input.as_bytes()).into()), expected);
        }
        assert_eq!(signed_hex([0; 20]), "0");
        assert_eq!(signed_hex([255; 20]), "-1");
    }

    #[test]
    fn official_profile_must_match_both_requested_name_and_claimed_uuid() {
        let uuid = Uuid::parse_str("37e14cb6-7fad-4874-a566-c6da67092d93").unwrap();
        let profile = || SessionProfile {
            name: "JavaShared".into(),
            id: uuid.simple().to_string(),
        };
        assert_eq!(profile().verify("javashared", uuid).unwrap().uuid, uuid);
        assert!(profile().verify("Other", uuid).is_err());
        assert!(profile().verify("JavaShared", Uuid::new_v4()).is_err());
        assert!(
            SessionProfile {
                name: "JavaShared".into(),
                id: "0".repeat(32)
            }
            .verify("JavaShared", Uuid::nil())
            .is_err()
        );
        assert!(
            serde_json::from_str::<SessionProfile>(r#"{"id":"x","id":"y","name":"a"}"#).is_err()
        );
    }

    #[tokio::test]
    async fn encrypted_login_uses_verified_identity_and_rejects_invalid_challenge_or_profile() {
        use crate::protocol::java::{Preview, test_client::Probe};
        use tokio::sync::watch;
        let authentication = Authentication::new().await.unwrap();
        let uuid = Uuid::parse_str("37e14cb6-7fad-4874-a566-c6da67092d93").unwrap();
        for (bad_challenge, bad_profile) in [(false, false), (true, false), (false, true)] {
            let mut auth = authentication.clone();
            let key = auth.public_key.clone();
            auth.verifier = Some(Arc::new(move |name, hash, claimed| {
                ensure!(
                    name == "JavaShared" && claimed == uuid,
                    "unexpected profile request"
                );
                ensure!(
                    hash == server_hash(&[0x42; 16], &key),
                    "client/server shared-secret hash mismatch"
                );
                SessionProfile {
                    name: name.into(),
                    id: if bad_profile { Uuid::new_v4() } else { uuid }
                        .simple()
                        .to_string(),
                }
                .verify(name, claimed)
            }));
            let (stop, shutdown) = watch::channel(false);
            let (world, owner) = crate::world::create(shutdown.clone());
            let mut joins = world.subscribe_game();
            let owner = tokio::spawn(owner);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (_, status) =
                watch::channel(sculith_core::ServerStatus::new("auth test".into(), 20).unwrap());
            let server = tokio::spawn(super::super::serve(
                listener,
                status,
                Some(Preview {
                    authentication: Some(Arc::new(auth)),
                    content: None,
                    world: world.clone(),
                    timeout: std::time::Duration::from_secs(10),
                    shutdown: shutdown.clone(),
                }),
                shutdown,
                crate::connections::Observer::default(),
            ));
            let result = Probe::enter_online(address, uuid, bad_challenge).await;
            if bad_challenge || bad_profile {
                assert!(result.is_err());
                assert_eq!(world.online_players(), 0);
            } else {
                let _client = result.unwrap();
                let joined = tokio::time::timeout(std::time::Duration::from_secs(3), joins.recv())
                    .await
                    .unwrap()
                    .unwrap();
                let crate::world::WorldEvent::Joined(joined) = joined.as_ref() else {
                    panic!("expected committed join");
                };
                let player = world.player_snapshot(joined.id).await.unwrap();
                assert_eq!(player.profile.uuid, uuid);
                assert_eq!(player.profile.subject, format!("java:{uuid}"));
                assert_eq!(world.online_players(), 1);
            }
            stop.send_replace(true);
            server.await.unwrap().unwrap();
            owner.await.unwrap().unwrap();
        }
    }
}
