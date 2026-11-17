//! One immutable Bedrock source and its vanilla Java representation.

use std::{io, net::IpAddr, sync::Arc, time::Duration};

use pumpkin_config::{AdvancedConfiguration, resource_pack::BedrockPack};
use sculith_packs::bundle::Bundle;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Semaphore,
    time::timeout,
};

const MAX_HTTP_HEADERS: usize = 16 * 1024;
const HTTP_CONNECTIONS: usize = 32;
const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(300);

pub struct SharedResources {
    pub bundle: Bundle,
    java_path: String,
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
    let bundle =
        tokio::task::spawn_blocking(move || Bundle::prepare(&shared.source, &shared.mapping))
            .await
            .map_err(io::Error::other)?
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    let java = &mut config.resource_pack.java;
    java.enabled = true;
    java.force = required;
    java.sha1.clone_from(&bundle.java.sha1);
    let bedrock = &mut config.resource_pack.bedrock;
    bedrock.enabled = true;
    bedrock.force = required;
    bedrock.packs = vec![BedrockPack {
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
    }];
    tracing::info!(
        textures = bundle.report.converted_textures,
        sounds = bundle.report.converted_sounds.len(),
        models = bundle.report.converted_models.len(),
        "Loaded shared resource pack"
    );
    Ok(Some(Arc::new(SharedResources {
        java_path: format!("/sculith/packs/{}/{}.zip", bundle.java_id, bundle.java.sha1),
        bundle,
        http_slots: Semaphore::new(HTTP_CONNECTIONS),
    })))
}

impl SharedResources {
    /// Resolve a shared Java sound identifier to its authored Bedrock event name.
    pub fn bedrock_sound_name<'a>(&'a self, sound_name: &'a str) -> &'a str {
        self.bundle
            .report
            .converted_sounds
            .iter()
            .find(|mapping| mapping.java == sound_name)
            .map_or(sound_name, |mapping| mapping.bedrock.as_str())
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
        if path != self.java_path {
            return Self::empty_response(stream, "404 Not Found").await;
        }
        let archive = &self.bundle.java;
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
            Some(pumpkin_config::resource_pack::SharedResourcePackConfig {
                source: root.join("pack"),
                mapping: root.join("textures.json"),
                required: true,
            });
        let prepared = prepare(&mut config).await.unwrap().unwrap();
        assert!(config.resource_pack.java.force);
        assert_eq!(config.resource_pack.java.sha1, prepared.bundle.java.sha1);
        assert_eq!(
            config.resource_pack.bedrock.packs[0].uuid,
            prepared.bundle.bedrock_id
        );
        assert_eq!(
            config.resource_pack.bedrock.packs[0].size,
            prepared.bundle.bedrock.bytes.len() as u64
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
    async fn same_port_get_head_and_unknown_paths_serve_only_the_frozen_archive() {
        let resources = resources().await;
        let path = &resources.java_path;
        let response = request(
            resources.clone(),
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes(),
        )
        .await;
        let boundary = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let header = std::str::from_utf8(&response[..boundary]).unwrap();
        assert!(header.starts_with("HTTP/1.1 200 OK"));
        assert!(header.contains(&format!(
            "Content-Length: {}",
            resources.bundle.java.bytes.len()
        )));
        assert_eq!(
            &response[boundary + 4..],
            resources.bundle.java.bytes.as_ref()
        );
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
        let resources = resources().await;
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
