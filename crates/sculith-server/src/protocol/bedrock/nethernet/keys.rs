//! Minecraft's signing authority for NetherNet offers, independent of game login.
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use nethernet::identity::{
    MINECRAFT_KEYS_URL, TokenTrust,
    jwk::{JwkSet, MinecraftKeys},
};
use std::{
    collections::HashSet,
    io::Read,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::{Semaphore, watch};

const MAX_BYTES: u64 = 64 * 1024;
const MAX_KEYS: usize = 16;
const RETRY: Duration = Duration::from_secs(60);
const REFRESH: Duration = Duration::from_secs(3600);
const STALE: Duration = Duration::from_secs(24 * 3600);

pub(super) struct Authority {
    keys: MinecraftKeys,
    http: ureq::Agent,
    schedule: Schedule,
}

impl Authority {
    pub async fn load() -> Result<Self> {
        let http = crate::protocol::http_client::authentication_metadata();
        let keys = fetch(http.clone())
            .await
            .context("cannot load NetherNet Microsoft signing keys")?;
        tracing::info!(
            keys = keys.keys.len(),
            "NetherNet Microsoft signing keys loaded"
        );
        Ok(Self {
            keys: keys.into(),
            http,
            schedule: Schedule::new(Instant::now()),
        })
    }

    pub fn trust(&self) -> TokenTrust {
        TokenTrust::Minecraft(self.keys.clone())
    }

    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {}
            }
            let now = Instant::now();
            self.schedule.missed |= self.keys.take_missed();
            if self.schedule.expired(now) {
                self.keys.replace(JwkSet::default());
                self.schedule.cleared = true;
                tracing::warn!(
                    "NetherNet signing key cache expired; new offers require refreshed keys"
                );
            }
            if !self.schedule.due(now) {
                continue;
            }
            self.schedule.attempt(now);
            // A refresh never runs in the signaling/game loop. Cancellation
            // retains its global worker permit until the bounded HTTP call ends.
            match fetch(self.http.clone()).await {
                Ok(keys) => {
                    self.keys.replace(keys);
                    self.schedule.succeeded(Instant::now());
                }
                Err(error) => {
                    self.schedule.missed = true;
                    tracing::warn!(%error, "NetherNet signing key refresh failed");
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn from_keys(keys: JwkSet) -> Self {
        Self {
            keys: keys.into(),
            http: crate::protocol::http_client::authentication_metadata(),
            schedule: Schedule::new(Instant::now()),
        }
    }
}

async fn fetch(http: ureq::Agent) -> Result<JwkSet> {
    blocking(move || {
        let mut response = http
            .get(MINECRAFT_KEYS_URL)
            .config()
            .max_redirects(0)
            .build()
            .call()
            .context("NetherNet signing key HTTPS request failed")?;
        ensure!(
            response.status().as_u16() == 200,
            "NetherNet signing key endpoint did not return 200"
        );
        let mut body = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(MAX_BYTES + 1)
            .read_to_end(&mut body)?;
        parse(&body)
    })
    .await
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    static WORKERS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let permit = WORKERS
        .get_or_init(|| Arc::new(Semaphore::new(2)))
        .clone()
        .try_acquire_owned()
        .context("NetherNet key fetch workers are busy")?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
    .context("NetherNet key fetch worker failed")?
}

fn parse(body: &[u8]) -> Result<JwkSet> {
    ensure!(
        body.len() as u64 <= MAX_BYTES,
        "NetherNet signing key response exceeds limit"
    );
    let mut set: JwkSet =
        serde_json::from_slice(body).context("invalid NetherNet signing key JSON")?;
    ensure!(
        (1..=MAX_KEYS).contains(&set.keys.len()),
        "invalid NetherNet signing key count"
    );
    set.keys.retain(|key| key.kty == "RSA");
    ensure!(
        !set.keys.is_empty(),
        "NetherNet signing key set has no RSA keys"
    );
    let mut kids = HashSet::new();
    for key in &set.keys {
        let kid = key
            .kid
            .as_deref()
            .context("NetherNet signing key has no identifier")?;
        ensure!(
            !kid.is_empty()
                && kid.len() <= 128
                && kid.is_ascii()
                && !kid.chars().any(char::is_control)
                && kids.insert(kid),
            "invalid or duplicate NetherNet signing key identifier"
        );
        let n =
            URL_SAFE_NO_PAD.decode(key.n.as_deref().context("NetherNet key has no modulus")?)?;
        let e =
            URL_SAFE_NO_PAD.decode(key.e.as_deref().context("NetherNet key has no exponent")?)?;
        ensure!(
            (256..=1024).contains(&n.len())
                && n[0] != 0
                && (n.len() > 256 || n[0] >= 128)
                && n[n.len() - 1] & 1 == 1,
            "invalid NetherNet RSA modulus"
        );
        ensure!(
            (1..=4).contains(&e.len()) && e[0] != 0,
            "invalid NetherNet RSA exponent"
        );
        let exponent = e
            .iter()
            .fold(0u32, |value, byte| (value << 8) | u32::from(*byte));
        ensure!(
            exponent >= 3 && exponent & 1 == 1,
            "invalid NetherNet RSA exponent"
        );
    }
    Ok(set)
}

struct Schedule {
    last_attempt: Instant,
    last_success: Instant,
    missed: bool,
    cleared: bool,
}
impl Schedule {
    fn new(now: Instant) -> Self {
        Self {
            last_attempt: now,
            last_success: now,
            missed: false,
            cleared: false,
        }
    }
    fn due(&self, now: Instant) -> bool {
        now.duration_since(self.last_attempt) >= RETRY
            && (self.missed || now.duration_since(self.last_success) >= REFRESH)
    }
    fn attempt(&mut self, now: Instant) {
        self.last_attempt = now;
        self.missed = false;
    }
    fn succeeded(&mut self, now: Instant) {
        self.last_success = now;
        self.cleared = false;
    }
    fn expired(&self, now: Instant) -> bool {
        !self.cleared && now.duration_since(self.last_success) >= STALE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nethernet::identity::jwk::Jwk;

    fn key(kid: &str) -> Jwk {
        Jwk {
            kty: "RSA".into(),
            kid: Some(kid.into()),
            n: Some(URL_SAFE_NO_PAD.encode([255; 256])),
            e: Some("AQAB".into()),
        }
    }

    fn encoded(keys: Vec<Jwk>) -> Vec<u8> {
        serde_json::to_vec(&JwkSet { keys }).unwrap()
    }

    #[test]
    fn only_bounded_identified_rsa_keys_are_installed() {
        let valid = key("current");
        assert_eq!(parse(&encoded(vec![valid.clone()])).unwrap().keys.len(), 1);
        assert!(parse(&encoded(vec![])).is_err());
        assert!(parse(&encoded(vec![valid.clone(); MAX_KEYS + 1])).is_err());
        assert!(parse(&encoded(vec![valid.clone(), valid.clone()])).is_err());
        assert!(parse(&vec![b' '; MAX_BYTES as usize + 1]).is_err());
        assert!(parse(b"{malformed}").is_err());
        for replacement in [
            None,
            Some("".into()),
            Some("x".repeat(129)),
            Some("bad\nkey".into()),
        ] {
            let mut invalid = valid.clone();
            invalid.kid = replacement;
            assert!(parse(&encoded(vec![invalid])).is_err());
        }
        let mut unsupported = valid.clone();
        unsupported.kty = "EC".into();
        assert!(parse(&encoded(vec![unsupported.clone()])).is_err());
        assert_eq!(
            parse(&encoded(vec![unsupported, valid]))
                .unwrap()
                .keys
                .len(),
            1
        );
    }

    #[test]
    fn malformed_and_weak_rsa_components_are_rejected() {
        for component in [
            None,
            Some("!".into()),
            Some(URL_SAFE_NO_PAD.encode([255; 255])),
            Some(URL_SAFE_NO_PAD.encode([0; 256])),
            Some(URL_SAFE_NO_PAD.encode([128; 256])),
            Some(URL_SAFE_NO_PAD.encode([255; 1025])),
        ] {
            let mut invalid = key("a");
            invalid.n = component;
            assert!(parse(&encoded(vec![invalid])).is_err());
        }
        for component in [
            None,
            Some("!".into()),
            Some("".into()),
            Some(URL_SAFE_NO_PAD.encode([1])),
            Some(URL_SAFE_NO_PAD.encode([2])),
            Some(URL_SAFE_NO_PAD.encode([0, 3])),
            Some(URL_SAFE_NO_PAD.encode([255; 5])),
        ] {
            let mut invalid = key("a");
            invalid.e = component;
            assert!(parse(&encoded(vec![invalid])).is_err());
        }
    }

    #[test]
    fn misses_refresh_after_cooldown_and_failed_refreshes_remain_due() {
        let now = Instant::now();
        let mut state = Schedule::new(now);
        state.missed = true;
        assert!(!state.due(now + RETRY - Duration::from_nanos(1)));
        assert!(state.due(now + RETRY));
        state.attempt(now + RETRY);
        state.missed = true; // Failed fetch retains the pending refresh.
        assert!(!state.due(now + RETRY + Duration::from_secs(1)));
        assert!(state.due(now + RETRY * 2));
        state.attempt(now + RETRY * 2);
        state.succeeded(now + RETRY * 2);
        assert!(!state.due(now + RETRY * 3));
        assert!(state.due(now + RETRY * 2 + REFRESH));
    }

    #[test]
    fn stale_keys_are_discarded_once_and_success_rearms_expiry() {
        let now = Instant::now();
        let mut state = Schedule::new(now);
        assert!(!state.expired(now + STALE - Duration::from_nanos(1)));
        assert!(state.expired(now + STALE));
        state.cleared = true;
        assert!(!state.expired(now + STALE + REFRESH));
        state.succeeded(now + STALE + REFRESH);
        assert!(!state.expired(now + STALE + REFRESH));
        assert!(state.expired(now + STALE * 2 + REFRESH));
    }

    #[tokio::test]
    async fn cancelling_waiters_does_not_release_running_fetch_workers() {
        let mut jobs = Vec::new();
        let mut releases = Vec::new();
        for _ in 0..2 {
            let (ready, started) = tokio::sync::oneshot::channel();
            let (release, wait) = std::sync::mpsc::channel();
            jobs.push(tokio::spawn(blocking(move || {
                let _ = ready.send(());
                wait.recv_timeout(Duration::from_secs(5))?;
                Ok(())
            })));
            releases.push(release);
            tokio::time::timeout(Duration::from_secs(2), started)
                .await
                .unwrap()
                .unwrap();
        }
        for job in jobs {
            job.abort();
            assert!(job.await.unwrap_err().is_cancelled());
        }
        assert!(
            blocking(|| Ok(())).await.is_err(),
            "cancellation released an active worker's permit"
        );
        for release in releases {
            release.send(()).unwrap();
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while blocking(|| Ok(())).await.is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
