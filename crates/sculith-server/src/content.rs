//! Loopback delivery of an immutable, content-addressed Java pack.
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use http::{Method, Request, Response, StatusCode};
use http_body_util::Full;
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use sculith_packs::bundle::Bundle;
use tokio::{
    net::TcpListener,
    sync::{Semaphore, watch},
    task::JoinSet,
};

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
    pub async fn prepare(address: SocketAddr, bundle: Bundle) -> Result<Self> {
        ensure!(address.ip().is_loopback(), "pack delivery is loopback-only");
        let listener = TcpListener::bind(address)
            .await
            .context("cannot bind pack HTTP listener")?;
        let path = format!("/packs/{}.zip", bundle.java.sha1);
        let java_url = format!("http://{}{path}", listener.local_addr()?);
        Ok(Self {
            listener,
            content: Arc::new(Content {
                bundle,
                java_url,
                path,
            }),
        })
    }

    pub async fn serve(self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let slots = Arc::new(Semaphore::new(8));
        let mut clients = JoinSet::new();
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
                accepted = self.listener.accept() => {
                    let (stream, remote) = accepted?;
                    if !remote.ip().is_loopback() { continue; }
                    let Ok(permit) = slots.clone().try_acquire_owned() else { continue; };
                    let content = self.content.clone();
                    clients.spawn(async move {
                        let _permit = permit;
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
