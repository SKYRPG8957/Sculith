//! Delivery of immutable editions compiled from the authored Bedrock source.

use std::{
    collections::{HashSet, VecDeque},
    io,
    net::IpAddr,
    sync::{Arc, Mutex, RwLock, TryLockError, Weak},
    time::Duration,
};

use bytes::Bytes;
use pumpkin_nbt::{Nbt, NbtCompound, deserializer::NbtReadHelperBedrock, tag::NbtTag};
use sculith_config::{AdvancedConfiguration, resource_pack::BedrockPack};
pub use sculith_packs::bundle::PackEdit;
use sculith_packs::{
    ModelMapping, SoundMapping,
    bundle::{Bundle, PackSource},
};
use sculith_protocol::bedrock::{
    SubClient, client::VANILLA_ACTOR_IDENTIFIERS, packet_encoder::BedrockBatchEncoder,
};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{OnceCell, Semaphore},
    time::timeout,
};

use crate::server::Server;

const MAX_HTTP_HEADERS: usize = 16 * 1024;
const HTTP_CONNECTIONS: usize = 32;
const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_LIVE_REVISIONS: usize = 4;
// One cold download advances by a window of at most fourteen NetherNet segments.
pub(crate) const BEDROCK_PACK_CHUNK_SIZE: u32 = 128 * 1024;

struct PackFrame {
    digest: [u8; 32],
    encoder: BedrockBatchEncoder,
    bytes: Arc<OnceCell<Bytes>>,
}

pub struct SharedPack {
    pub bundle: Bundle,
    pub(crate) bedrock: BedrockPack,
    java_path: String,
    frames: Mutex<VecDeque<PackFrame>>,
}

struct Revisions {
    source: PackSource,
    active: Arc<SharedPack>,
    retired: Vec<Weak<SharedPack>>,
}

pub struct SharedResources {
    revisions: RwLock<Revisions>,
    editing: Mutex<()>,
    // The authored files can change. Bindings stay stable for clients with older packs.
    models: Vec<ModelMapping>,
    sounds: Vec<SoundMapping>,
    pub actor_identifiers: Bytes,
    http_slots: Semaphore,
}

/// Compile before listeners or worlds are opened.
///
/// # Errors
/// Returns an error for conflicting edition settings, unsupported proxy delivery,
/// invalid source assets, or a failed compiler task.
pub async fn prepare(
    config: &mut AdvancedConfiguration,
) -> io::Result<Option<Arc<SharedResources>>> {
    let Some(shared) = config.resource_pack.shared.clone() else {
        return Ok(None);
    };
    if config.resource_pack.java.enabled || config.resource_pack.bedrock.enabled {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "shared resource packs cannot be combined with edition-specific packs",
        ));
    }
    if config.networking.proxy.enabled {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "shared resource delivery currently requires a direct Java listener",
        ));
    }
    let required = shared.required;
    let (source, bundle) = tokio::task::spawn_blocking(move || {
        let source = PackSource::read(&shared.source, &shared.mapping)?;
        source.compile().map(|bundle| (source, bundle))
    })
    .await
    .map_err(io::Error::other)?
    .map_err(invalid_pack)?;
    let pack = Arc::new(SharedPack::new(bundle));
    let java = &mut config.resource_pack.java;
    java.enabled = true;
    java.force = required;
    java.sha1.clone_from(&pack.bundle.java.sha1);
    let bedrock = &mut config.resource_pack.bedrock;
    bedrock.enabled = true;
    bedrock.force = required;
    bedrock.packs = vec![pack.bedrock.clone()];
    let report = &pack.bundle.report;
    tracing::info!(
        textures = report.converted_textures,
        sounds = report.converted_sounds.len(),
        models = report.converted_models.len(),
        "Loaded shared resource pack"
    );
    let actor_identifiers = actor_identifiers(&report.converted_models)?;
    Ok(Some(Arc::new(SharedResources {
        models: report.converted_models.clone(),
        sounds: report.converted_sounds.clone(),
        revisions: RwLock::new(Revisions {
            source,
            active: pack,
            retired: Vec::new(),
        }),
        editing: Mutex::new(()),
        actor_identifiers,
        http_slots: Semaphore::new(HTTP_CONNECTIONS),
    })))
}

fn invalid_pack(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("shared resource pack: {error:#}"),
    )
}

fn actor_identifiers(models: &[sculith_packs::ModelMapping]) -> io::Result<Bytes> {
    if models.is_empty() {
        return Ok(Bytes::from_static(VANILLA_ACTOR_IDENTIFIERS));
    }
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "Invalid actor registry");
    let mut reader = NbtReadHelperBedrock::new(io::Cursor::new(VANILLA_ACTOR_IDENTIFIERS));
    let mut registry = Nbt::read(&mut reader).map_err(io::Error::other)?;
    let Some(NbtTag::List(actors)) = registry.root_tag.child_tags.get_mut("idlist") else {
        return Err(invalid());
    };
    let mut names: HashSet<String> = actors
        .iter()
        .filter_map(NbtTag::extract_compound)
        .filter_map(|actor| actor.get_string("id").map(str::to_owned))
        .collect();
    // Geyser reserves this range for custom actors; vanilla actor IDs stay intact.
    let mut runtime_id = actors
        .iter()
        .filter_map(NbtTag::extract_compound)
        .filter_map(|actor| actor.get_int("rid"))
        .max()
        .unwrap_or(0)
        .max(99_999);
    for model in models {
        if !names.insert(model.bedrock.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Duplicate actor identifier: {}", model.bedrock),
            ));
        }
        runtime_id = runtime_id.checked_add(1).ok_or_else(invalid)?;
        let mut actor = NbtCompound::new();
        actor.put_string("id", model.bedrock.clone());
        actor.put_string("bid", String::new());
        actor.put_int("rid", runtime_id);
        actor.put_bool("hasspawnegg", false);
        actor.put_bool("summonable", true);
        actor.put_bool("experimental", false);
        actors.push(NbtTag::Compound(actor));
    }
    Ok(registry.write_bedrock())
}

impl SharedResources {
    pub fn snapshot(&self) -> Arc<SharedPack> {
        self.revisions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
            .clone()
    }

    fn apply_edits(&self, edits: Vec<PackEdit>) -> io::Result<Arc<SharedPack>> {
        let _editing = match self.editing.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "pack edit in progress",
                ));
            }
        };
        let source = self
            .revisions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .source
            .with_edits(edits)
            .map_err(invalid_pack)?;
        let bundle = source.compile().map_err(invalid_pack)?;
        let mut revisions = self
            .revisions
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if bundle.bedrock_id == revisions.active.bundle.bedrock_id
            && bundle.java_id == revisions.active.bundle.java_id
        {
            return Ok(revisions.active.clone());
        }
        revisions.retired.retain(|pack| pack.strong_count() > 0);
        if Arc::strong_count(&revisions.active) > 1 {
            if revisions.retired.len() >= MAX_LIVE_REVISIONS - 1 {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "too many pack revisions in use",
                ));
            }
            let retired = Arc::downgrade(&revisions.active);
            revisions.retired.push(retired);
        }
        let pack = Arc::new(SharedPack::new(bundle));
        revisions.active = pack.clone();
        revisions.source = source;
        Ok(pack)
    }

    pub fn model(&self, name: &str) -> Option<&ModelMapping> {
        self.models.iter().find(|mapping| mapping.java == name)
    }

    /// Resolve a shared Java sound identifier to its authored Bedrock event name.
    pub fn bedrock_sound_name<'a>(&'a self, sound_name: &'a str) -> &'a str {
        self.sounds
            .iter()
            .find(|mapping| mapping.java == sound_name)
            .map_or(sound_name, |mapping| mapping.bedrock.as_str())
    }
}

impl Server {
    /// Compile a batch of Bedrock file edits for future joins.
    ///
    /// Edits are in memory and retain the configured Java bindings. Connections
    /// already negotiating finish with their original revision.
    ///
    /// # Errors
    /// Rejects a missing shared pack, invalid edits, or too many revisions in use.
    pub async fn edit_shared_resources(&self, edits: Vec<PackEdit>) -> io::Result<Arc<SharedPack>> {
        let resources = self.shared_resources.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "shared resource pack is not configured",
            )
        })?;
        // Native plugins can be linked to a separate copy of Tokio.
        self.runtime
            .spawn_blocking(move || resources.apply_edits(edits))
            .await
            .map_err(io::Error::other)?
    }
}

impl SharedPack {
    fn new(bundle: Bundle) -> Self {
        Self {
            bedrock: BedrockPack {
                uuid: bundle.bedrock_id,
                version: bundle.bedrock_version.clone(),
                size: bundle.bedrock.bytes.len() as u64,
                download_url: String::new(),
                path: None,
                content_key: String::new(),
                sub_pack_name: String::new(),
                content_id: String::new(),
                has_scripts: false,
                addon_pack: false,
                rtx_enabled: false,
            },
            java_path: format!("/sculith/packs/{}/{}.zip", bundle.java_id, bundle.java.sha1),
            frames: Mutex::default(),
            bundle,
        }
    }

    /// Share identical, translated pack chunks between simultaneous downloads.
    /// The key covers the actual packet body and the negotiated wire encoding.
    pub(crate) async fn chunk_frame(
        &self,
        encoder: BedrockBatchEncoder,
        body: &Bytes,
    ) -> io::Result<Bytes> {
        let digest = Sha256::digest(body).into();
        let cell = {
            let mut frames = self
                .frames
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let cached = frames
                .iter()
                .find(|f| f.digest == digest && f.encoder == encoder)
                .map(|frame| frame.bytes.clone());
            cached.unwrap_or_else(|| {
                // At most 8 MiB of chunk contents per immutable pack revision.
                if frames.len() == 64 {
                    frames.pop_front();
                }
                let bytes = Arc::new(OnceCell::new());
                frames.push_back(PackFrame {
                    digest,
                    encoder,
                    bytes: bytes.clone(),
                });
                bytes
            })
        };
        cell.get_or_try_init(|| async {
            let body = body.clone();
            tokio::task::spawn_blocking(move || {
                let mut data = Vec::new();
                encoder.write_game_packet(
                    pumpkin_data::bedrock_packet::RESOURCE_PACK_CHUNK_DATA as u16,
                    SubClient::Main,
                    SubClient::Main,
                    &body,
                    &mut data,
                )?;
                Ok::<_, io::Error>(Bytes::from(data).slice(1..))
            })
            .await
            .map_err(io::Error::other)?
        })
        .await
        .cloned()
    }

    /// Derive the download address from the address used to join this server.
    ///
    /// # Errors
    /// Rejects invalid or forwarded host fields instead of emitting an unrelated URL.
    pub fn java_url(&self, address: &str, port: u16) -> io::Result<String> {
        if port == 0
            || address.is_empty()
            || address.len() > 255
            || address.chars().any(|c| c.is_control() || c.is_whitespace())
            || address.contains(['/', '\\', '@', '?', '#'])
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid resource pack host",
            ));
        }
        let host = match address.parse::<IpAddr>() {
            Ok(IpAddr::V6(ip)) => format!("[{ip}]"),
            _ => address.to_owned(),
        };
        let url = reqwest::Url::parse(&format!("http://{host}:{port}{}", self.java_path))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid resource pack host",
            ));
        }
        Ok(url.to_string())
    }
}

impl SharedResources {
    fn pack_for_path(&self, path: &str) -> Option<Arc<SharedPack>> {
        let revisions = self
            .revisions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if path == revisions.active.java_path {
            return Some(revisions.active.clone());
        }
        revisions
            .retired
            .iter()
            .filter_map(Weak::upgrade)
            .find(|pack| path == pack.java_path)
    }

    /// Serve pack HTTP on the Java port, or return the consumed Minecraft prefix.
    ///
    /// # Errors
    /// Returns transport errors and bounded header/response timeouts.
    pub async fn route_http(&self, stream: &mut TcpStream) -> io::Result<Option<[u8; 4]>> {
        let mut prefix = [0; 4];
        timeout(HEADER_TIMEOUT, stream.read_exact(&mut prefix)).await??;
        if &prefix != b"GET " && &prefix != b"HEAD" {
            return Ok(Some(prefix));
        }
        timeout(RESPONSE_TIMEOUT, self.serve_http(stream, prefix)).await??;
        Ok(None)
    }

    async fn serve_http(&self, stream: &mut TcpStream, prefix: [u8; 4]) -> io::Result<()> {
        let Ok(_permit) = self.http_slots.try_acquire() else {
            return Self::empty_response(stream, "503 Service Unavailable").await;
        };
        let header = timeout(HEADER_TIMEOUT, async {
            let mut header = Vec::from(prefix);
            while !header.windows(4).any(|window| window == b"\r\n\r\n") {
                if header.len() == MAX_HTTP_HEADERS {
                    return Ok::<_, io::Error>(None);
                }
                let mut chunk = [0; 1024];
                let limit = chunk.len().min(MAX_HTTP_HEADERS - header.len());
                let count = stream.read(&mut chunk[..limit]).await?;
                if count == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                header.extend_from_slice(&chunk[..count]);
            }
            Ok(Some(header))
        })
        .await??;
        let Some(header) = header else {
            return Self::empty_response(stream, "431 Request Header Fields Too Large").await;
        };
        let Ok(header) = std::str::from_utf8(&header) else {
            return Self::empty_response(stream, "400 Bad Request").await;
        };
        let mut fields = header
            .split("\r\n")
            .next()
            .unwrap_or_default()
            .split_ascii_whitespace();
        let (Some(method), Some(path), Some(version), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Self::empty_response(stream, "400 Bad Request").await;
        };
        if !matches!(method, "GET" | "HEAD") || !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
            return Self::empty_response(stream, "400 Bad Request").await;
        }
        let Some(pack) = self.pack_for_path(path) else {
            return Self::empty_response(stream, "404 Not Found").await;
        };
        let archive = &pack.bundle.java;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: {}\r\nETag: \"{}\"\r\nCache-Control: public, max-age=31536000, immutable\r\nConnection: close\r\n\r\n",
            archive.bytes.len(),
            archive.sha1
        );
        stream.write_all(response.as_bytes()).await?;
        if method == "GET" {
            stream.write_all(&archive.bytes).await?;
        }
        stream.shutdown().await
    }

    async fn empty_response(stream: &mut TcpStream, status: &str) -> io::Result<()> {
        stream
            .write_all(
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await?;
        stream.shutdown().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tokio::net::TcpListener;

    async fn resources() -> Arc<SharedResources> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../sculith-packs/tests/fixtures");
        let mut config = AdvancedConfiguration::default();
        config.resource_pack.shared =
            Some(sculith_config::resource_pack::SharedResourcePackConfig {
                source: root.join("pack"),
                mapping: root.join("textures.json"),
                required: true,
            });
        let prepared = prepare(&mut config).await.unwrap().unwrap();
        let pack = prepared.snapshot();
        assert!(config.resource_pack.java.force);
        assert_eq!(config.resource_pack.java.sha1, pack.bundle.java.sha1);
        assert_eq!(
            config.resource_pack.bedrock.packs[0].uuid,
            pack.bundle.bedrock_id
        );
        assert_eq!(
            config.resource_pack.bedrock.packs[0].size,
            pack.bundle.bedrock.bytes.len() as u64
        );
        prepared
    }

    async fn request(resources: Arc<SharedResources>, bytes: &[u8]) -> Vec<u8> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(resources.route_http(&mut stream).await.unwrap().is_none());
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        // Make the initial HTTP discriminator span separate writes.
        client.write_all(&bytes[..1]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        client.write_all(&bytes[1..]).await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        server.await.unwrap();
        response
    }

    #[tokio::test]
    async fn identical_pack_frames_share_storage_per_wire_encoding() {
        let pack = resources().await.snapshot();
        let body = Bytes::from(vec![0x61; 4096]);
        let mut encoder = BedrockBatchEncoder::new();
        encoder.set_compression((256, 4));
        let (first, second) = tokio::join!(
            pack.chunk_frame(encoder, &body),
            pack.chunk_frame(encoder, &body)
        );
        let first = first.unwrap();
        assert_eq!(first.as_ptr(), second.unwrap().as_ptr());
        assert_eq!(first[0], 0);
        let different = pack
            .chunk_frame(encoder, &Bytes::from(vec![0x62; 4096]))
            .await
            .unwrap();
        assert_ne!(first, different);
        encoder.set_compression((0, 4));
        let plain = pack.chunk_frame(encoder, &body).await.unwrap();
        assert_eq!(plain[0], 0xff);
        assert!(plain.ends_with(&body));
        encoder.set_prefixed_compression(false);
        let legacy = pack.chunk_frame(encoder, &body).await.unwrap();
        assert_eq!(legacy, plain.slice(1..));
    }

    #[tokio::test]
    async fn same_port_get_head_and_unknown_paths_serve_only_the_frozen_archive() {
        let resources = resources().await;
        let pack = resources.snapshot();
        let path = &pack.java_path;
        let response = request(
            resources.clone(),
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes(),
        )
        .await;
        let boundary = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let header = std::str::from_utf8(&response[..boundary]).unwrap();
        assert!(header.starts_with("HTTP/1.1 200 OK"));
        assert!(header.contains(&format!("Content-Length: {}", pack.bundle.java.bytes.len())));
        assert_eq!(&response[boundary + 4..], pack.bundle.java.bytes.as_ref());
        let head = request(
            resources.clone(),
            format!("HEAD {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes(),
        )
        .await;
        assert!(head.starts_with(b"HTTP/1.1 200 OK"));
        assert!(head.ends_with(b"\r\n\r\n"));
        assert!(!head.windows(2).any(|w| w == b"PK"));
        let missing = request(
            resources,
            b"GET /../level.dat HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(missing.starts_with(b"HTTP/1.1 404 Not Found"));
    }

    #[tokio::test]
    async fn routing_preserves_a_fragmented_minecraft_prefix_and_following_bytes() {
        let resources = resources().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let prefix = resources.route_http(&mut stream).await.unwrap().unwrap();
            let (read, mut write) = stream.into_split();
            let mut read = std::io::Cursor::new(prefix).chain(read);
            let mut bytes = [0; 7];
            read.read_exact(&mut bytes).await.unwrap();
            write.write_all(&bytes).await.unwrap();
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        let packet = [16, 0, 137, 6, 1, 2, 3];
        client.write_all(&packet[..1]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        client.write_all(&packet[1..]).await.unwrap();
        let mut echoed = [0; 7];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(echoed, packet);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn urls_use_the_join_host_and_port_without_accepting_forwarded_or_injected_hosts() {
        let resources = resources().await.snapshot();
        assert!(
            resources
                .java_url("example.org", 25566)
                .unwrap()
                .starts_with("http://example.org:25566/sculith/packs/")
        );
        assert!(
            resources
                .java_url("::1", 25565)
                .unwrap()
                .starts_with("http://[::1]:25565/sculith/packs/")
        );
        for host in [
            "",
            "bad\0forwarded",
            "a@other",
            "a/other",
            "a#other",
            "a?other",
            "a\r\nHost:b",
            "a:123",
        ] {
            assert!(resources.java_url(host, 25565).is_err(), "{host:?}");
        }
        assert!(resources.java_url("localhost", 0).is_err());
    }
}
