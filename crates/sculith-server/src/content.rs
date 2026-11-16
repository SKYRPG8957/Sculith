//! Bounded delivery of an immutable, content-addressed Java pack.
use std::{
    convert::Infallible,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use http::{Method, Request, Response, StatusCode};
use http_body_util::Full;
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use sculith_packs::bundle::Bundle;
use tokio::{net::TcpListener, sync::watch, task::JoinSet};
use url::{Host, Url};

#[derive(Debug)]
pub struct PackBaseUrl(Url);

impl PackBaseUrl {
    pub fn parse(value: &str) -> Result<Self> {
        ensure!(
            value.len() <= 1024 && !value.chars().any(|c| c.is_whitespace() || c.is_control()),
            "content.public_url must be at most 1024 bytes without whitespace or controls"
        );
        let url =
            Url::parse(value).context("content.public_url must be an absolute HTTP(S) URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https") && url.host().is_some(),
            "content.public_url must be an absolute HTTP(S) URL"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "content.public_url cannot contain credentials, a query or a fragment"
        );
        let ip = match url.host() {
            Some(Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
            Some(Host::Ipv6(ip)) => Some(ip.to_canonical()),
            _ => None,
        };
        ensure!(
            url.port() != Some(0)
                && ip.is_none_or(|ip| !ip.is_unspecified()
                    && !ip.is_multicast()
                    && ip != IpAddr::V4(std::net::Ipv4Addr::BROADCAST)),
            "content.public_url requires a usable host and port"
        );
        Ok(Self(url))
    }

    pub fn is_loopback(&self) -> bool {
        match self.0.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.to_canonical().is_loopback(),
            Some(Host::Domain(host)) => {
                let host = host.trim_end_matches('.');
                host == "localhost" || host.ends_with(".localhost")
            }
            None => false,
        }
    }

    fn pack_url(&self, sha1: &str) -> String {
        let mut url = self.0.clone();
        url.path_segments_mut()
            .expect("validated HTTP(S) base")
            .pop_if_empty()
            .push("packs")
            .push(&format!("{sha1}.zip"));
        url.into()
    }
}

pub struct Content {
    pub bundle: Bundle,
    pub java_url: String,
    path: String,
}

pub struct Delivery {
    listener: TcpListener,
    pub content: Arc<Content>,
}

impl Delivery {
    pub async fn prepare(
        address: SocketAddr,
        public_url: Option<&str>,
        bundle: Bundle,
    ) -> Result<Self> {
        let public_url = public_url.map(PackBaseUrl::parse).transpose()?;
        ensure!(
            address.ip().is_loopback() || public_url.is_some(),
            "non-loopback content.bind requires content.public_url"
        );
        let listener = TcpListener::bind(address)
            .await
            .context("cannot bind pack HTTP listener")?;
        let path = format!("/packs/{}.zip", bundle.java.sha1);
        let java_url = match public_url {
            Some(base) => base.pack_url(&bundle.java.sha1),
            None => format!("http://{}{path}", listener.local_addr()?),
        };
        Ok(Self {
            listener,
            content: Arc::new(Content {
                bundle,
                java_url,
                path,
            }),
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub async fn serve(self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let mut clients = JoinSet::new();
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
                accepted = self.listener.accept(), if clients.len() < 8 => {
                    let (stream, _) = accepted?;
                    let content = self.content.clone();
                    clients.spawn(async move {
                        let service = service_fn(move |request| {
                            let response = response(request, &content);
                            async move { Ok::<_, Infallible>(response) }
                        });
                        let _ = tokio::time::timeout(Duration::from_secs(10),
                            http1::Builder::new().keep_alive(false).max_buf_size(8192)
                                .serve_connection(TokioIo::new(stream), service)).await;
                    });
                }
                Some(result) = clients.join_next(), if !clients.is_empty() => {
                    result.context("pack HTTP worker failed")?;
                }
            }
        }
        clients.abort_all();
        while let Some(result) = clients.join_next().await {
            if let Err(error) = result {
                ensure!(error.is_cancelled(), "pack HTTP worker panicked");
            }
        }
        Ok(())
    }
}

fn response(request: Request<Incoming>, content: &Content) -> Response<Full<Bytes>> {
    let method = request.method();
    let path = request.uri().path_and_query().map(|path| path.as_str());
    let status = if method != Method::GET && method != Method::HEAD {
        StatusCode::METHOD_NOT_ALLOWED
    } else if request.headers().contains_key("transfer-encoding")
        || request
            .headers()
            .get("content-length")
            .is_some_and(|value| value != "0")
    {
        StatusCode::BAD_REQUEST
    } else if path != Some(content.path.as_str()) {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::OK
    };
    let length = if status == StatusCode::OK {
        content.bundle.java.bytes.len()
    } else {
        0
    };
    let bytes = if status == StatusCode::OK && method == Method::GET {
        Bytes::from_owner(content.bundle.java.bytes.clone())
    } else {
        Bytes::new()
    };
    let mut response = Response::new(Full::new(bytes));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        "content-length",
        length.to_string().parse().expect("numeric length"),
    );
    headers.insert(
        "content-type",
        http::HeaderValue::from_static("application/zip"),
    );
    headers.insert("connection", http::HeaderValue::from_static("close"));
    headers.insert("allow", http::HeaderValue::from_static("GET, HEAD"));
    if status == StatusCode::OK {
        headers.insert(
            "cache-control",
            http::HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
        headers.insert(
            "etag",
            format!("\"{}\"", content.bundle.java.sha1)
                .parse()
                .expect("hex hash"),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertised_pack_url_preserves_proxy_prefix_and_ipv6_authority() {
        for (base, expected) in [
            (
                "https://packs.example.com",
                "https://packs.example.com/packs/abc.zip",
            ),
            (
                "https://packs.example.com/shared",
                "https://packs.example.com/shared/packs/abc.zip",
            ),
            (
                "https://packs.example.com/shared/",
                "https://packs.example.com/shared/packs/abc.zip",
            ),
            (
                "http://[2001:db8::1]:8080/shared%20assets",
                "http://[2001:db8::1]:8080/shared%20assets/packs/abc.zip",
            ),
        ] {
            assert_eq!(PackBaseUrl::parse(base).unwrap().pack_url("abc"), expected);
        }
    }

    #[test]
    fn advertised_url_rejects_credentials_ambiguous_components_and_unusable_hosts() {
        for invalid in [
            "/packs",
            "file:///packs",
            "ftp://example.com",
            "https://",
            "http://example.com:0",
            "https://user:password@example.com",
            "https://example.com?token=secret",
            "https://example.com#ignored",
            "https://example.com/\nshared",
            " https://example.com",
            "http://0.0.0.0",
            "http://[::]",
            "http://[::ffff:0.0.0.0]",
            "http://224.0.0.1",
            "http://255.255.255.255",
            "http://[ff02::1]",
        ] {
            assert!(PackBaseUrl::parse(invalid).is_err(), "{invalid:?}");
        }
        assert!(PackBaseUrl::parse(&format!("https://example.com/{}", "x".repeat(1024))).is_err());
    }

    #[test]
    fn loopback_classification_covers_alternative_ipv4_and_mapped_ipv6() {
        for host in [
            "127.0.0.1",
            "127.1",
            "2130706433",
            "[::1]",
            "[::ffff:127.0.0.1]",
            "LOCALHOST",
            "localhost.",
            "packs.localhost",
        ] {
            assert!(
                PackBaseUrl::parse(&format!("http://{host}"))
                    .unwrap()
                    .is_loopback(),
                "{host}"
            );
        }
        for host in ["192.168.1.10", "example.com", "[2001:db8::1]"] {
            assert!(
                !PackBaseUrl::parse(&format!("https://{host}"))
                    .unwrap()
                    .is_loopback()
            );
        }
    }
}
