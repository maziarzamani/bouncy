//! Custom CA bundle support — `Fetcher::builder().ca_file(path)`.
//!
//! Two layers of test:
//!   1. Bogus PEM bytes → loud error from `build()`. Cheap.
//!   2. Full TLS handshake using a self-signed cert that's only trusted
//!      because we passed it via `ca_file`. Without `--ca-file` the
//!      handshake would fail; with it, the request succeeds.

use std::convert::Infallible;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;

use bouncy_fetch::{FetchRequest, Fetcher};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

#[test]
fn ca_file_with_garbage_bytes_is_an_error() {
    let dir = std::env::temp_dir().join(format!("bouncy-ca-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("bad.pem");
    std::fs::write(&p, b"not a certificate, not even close").unwrap();

    let r = Fetcher::builder().ca_file(&p).build();
    assert!(
        r.is_err(),
        "expected ca_file with garbage to fail, got: {:?}",
        r.is_ok()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

async fn spawn_self_signed_https(
    cert_pem: String,
    key_pem: String,
) -> (SocketAddr, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "bouncy-ca-srv-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cert_path = dir.join("cert.pem");
    {
        let mut f = std::fs::File::create(&cert_path).unwrap();
        f.write_all(cert_pem.as_bytes()).unwrap();
    }

    // Parse for the in-memory rustls server config.
    let cert_der = CertificateDer::from_pem_slice(cert_pem.as_bytes())
        .expect("parse server cert")
        .into_owned();
    let key_der = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).expect("parse server key");

    let mut server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("server cfg");
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (sock, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(sock).await else {
                    return;
                };
                let svc = service_fn(|_req: Request<Incoming>| async move {
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(200)
                            .body(Full::new(Bytes::from_static(b"ca-ok")))
                            .unwrap(),
                    )
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), svc)
                    .await;
            });
        }
    });
    (addr, cert_path)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ca_file_lets_us_trust_self_signed_certs() {
    // Generate a fresh self-signed cert + key for "localhost".
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_pem = cert.pem();
    let key_pem = signing_key.serialize_pem();

    let (addr, cert_path) = spawn_self_signed_https(cert_pem, key_pem).await;
    let url = format!("https://localhost:{}/x", addr.port());

    // With the cert added via ca_file, the handshake should succeed.
    let f = Fetcher::builder().ca_file(&cert_path).build().unwrap();
    let r = f.request(FetchRequest::new(&url)).await.unwrap();
    assert_eq!(r.status, 200, "request should succeed with --ca-file");
    assert_eq!(&r.body[..], b"ca-ok");

    // Without the cert, the handshake should NOT succeed (cert isn't in the
    // system trust store). We only assert the error path, not the exact
    // shape — different rustls versions phrase TLS errors differently.
    let f2 = Fetcher::new().unwrap();
    let r2 = f2.request(FetchRequest::new(&url)).await;
    assert!(
        r2.is_err(),
        "request should fail without --ca-file (got: {:?})",
        r2.as_ref().map(|r| r.status)
    );

    let _ = std::fs::remove_file(&cert_path);
    if let Some(parent) = cert_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}
