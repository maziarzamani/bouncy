//! End-to-end integration tests for bouncy-fetch.
//!
//! Spins up a tiny hyper server in-process, points the Fetcher at it,
//! asserts the round-trip works.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use bouncy_fetch::Fetcher;

/// Spawn a hyper test server on 127.0.0.1:0. Returns the bound address and a
/// counter of received requests.
async fn spawn_server() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_t = counter.clone();

    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let counter = counter_t.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        let body = match req.uri().path() {
                            "/hello" => Bytes::from_static(b"hello"),
                            "/big" => Bytes::from(vec![b'x'; 64 * 1024]),
                            _ => Bytes::from_static(b"ok"),
                        };
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .header("content-type", "text/plain")
                                .body(Full::new(body))
                                .unwrap(),
                        )
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });

    (addr, counter)
}

#[tokio::test]
async fn get_returns_status_and_body() {
    let (addr, _) = spawn_server().await;
    let f = Fetcher::new().expect("build fetcher");
    let resp = f.get(&format!("http://{}/hello", addr)).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(&resp.body[..], b"hello");
}

#[tokio::test]
async fn pool_reuses_connection_for_same_origin() {
    // Two sequential GETs against the same origin should hit the same TCP
    // connection. We verify by counting accept()s on the listener — the
    // pool keeps the conn alive, so the second request reuses it.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepts = Arc::new(AtomicUsize::new(0));
    let accepts_t = accepts.clone();

    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            accepts_t.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let svc = service_fn(|_| async move {
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(200)
                            .body(Full::new(Bytes::from_static(b"ok")))
                            .unwrap(),
                    )
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });

    let f = Fetcher::new().expect("build fetcher");
    for _ in 0..3 {
        let r = f.get(&format!("http://{}/x", addr)).await.unwrap();
        assert_eq!(r.status, 200);
    }
    // Allow time for the pool to settle.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(
        accepts.load(Ordering::SeqCst),
        1,
        "expected 1 TCP connection across 3 GETs (pooled), got {}",
        accepts.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn rejects_unsupported_scheme() {
    let f = Fetcher::new().expect("build fetcher");
    let err = f.get("ftp://example.com/x").await.unwrap_err();
    assert!(
        format!("{err}").to_lowercase().contains("scheme"),
        "expected scheme error, got: {err}"
    );
}
