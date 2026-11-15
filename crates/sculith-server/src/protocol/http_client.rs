//! Public authentication metadata: bounded HTTPS with OS certificate trust.
use std::time::Duration;
use ureq::tls::{RootCerts, TlsConfig};

pub(super) fn authentication_metadata() -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(Duration::from_secs(15)))
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::Arc,
        thread,
        time::Instant,
    };

    #[test]
    fn plaintext_metadata_is_rejected_before_any_network_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/keys", listener.local_addr().unwrap());
        let error = authentication_metadata().get(&url).call().unwrap_err();
        assert!(matches!(error, ureq::Error::RequireHttpsOnly(_)));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn an_untrusted_certificate_is_rejected_over_a_real_tls_socket() {
        let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![certified.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der())
                .into(),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("https://{}/keys", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "TLS client never connected");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let conn = rustls::ServerConnection::new(Arc::new(config)).unwrap();
            let mut stream = rustls::StreamOwned::new(conn, socket);
            // TLS writes may buffer before the client rejects our certificate.
            // Require an application request to distinguish certificate acceptance.
            let mut first = [0u8; 1];
            stream.read_exact(&mut first)?;
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        });
        let result = authentication_metadata()
            .get(&url)
            .config()
            .proxy(None)
            .timeout_global(Some(Duration::from_secs(3)))
            .build()
            .call();
        assert!(result.is_err(), "untrusted metadata endpoint was accepted");
        assert!(
            server.join().unwrap().is_err(),
            "client sent an HTTP request through untrusted TLS"
        );
    }
}
