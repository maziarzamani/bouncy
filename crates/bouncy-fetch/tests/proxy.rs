//! Tests for HTTP CONNECT proxy support.
//!
//! Spins up a tiny CONNECT proxy on 127.0.0.1:0 that accepts CONNECT,
//! opens a TCP connection to the target, replies `HTTP/1.1 200
//! Connection Established`, and pipes bytes both directions. Plus a real
//! upstream HTTP server. The Fetcher is built with the proxy URL and
//! asserted to reach the upstream via the proxy — we count CONNECT
//! requests on the proxy and GETs on the upstream.

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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use bouncy_fetch::{FetchRequest, Fetcher};

/// Spawn an upstream HTTP server.
async fn spawn_upstream() -> (SocketAddr, Arc<AtomicUsize>) {
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
                let svc = service_fn(move |_req: Request<Incoming>| {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .body(Full::new(Bytes::from_static(b"upstream-ok")))
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

/// Spawn a CONNECT-only proxy.
async fn spawn_proxy() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connects = Arc::new(AtomicUsize::new(0));
    let connects_t = connects.clone();
    tokio::spawn(async move {
        loop {
            let (mut client, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let connects = connects_t.clone();
            tokio::spawn(async move {
                // Read request line (CONNECT host:port HTTP/1.1) + headers.
                let mut buf_client = BufReader::new(&mut client);
                let mut request_line = String::new();
                if buf_client.read_line(&mut request_line).await.is_err() {
                    return;
                }
                // Drain headers.
                loop {
                    let mut line = String::new();
                    if buf_client.read_line(&mut line).await.is_err() {
                        return;
                    }
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }

                if !request_line.starts_with("CONNECT ") {
                    let _ = client.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
                    return;
                }
                let parts: Vec<&str> = request_line.split_whitespace().collect();
                if parts.len() < 2 {
                    return;
                }
                let target = parts[1].to_string();
                connects.fetch_add(1, Ordering::SeqCst);

                // Open the upstream TCP and pipe both ways.
                let upstream = match TcpStream::connect(&target).await {
                    Ok(s) => s,
                    Err(_) => {
                        let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                        return;
                    }
                };
                let _ = client
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await;

                let (mut cr, mut cw) = client.into_split();
                let (mut ur, mut uw) = upstream.into_split();
                let c2u = async move {
                    let _ = tokio::io::copy(&mut cr, &mut uw).await;
                };
                let u2c = async move {
                    let _ = tokio::io::copy(&mut ur, &mut cw).await;
                };
                tokio::join!(c2u, u2c);
            });
        }
    });
    (addr, connects)
}

#[tokio::test]
async fn http_get_via_proxy() {
    let (upstream, upstream_count) = spawn_upstream().await;
    let (proxy, connect_count) = spawn_proxy().await;

    let f = Fetcher::builder()
        .proxy(format!("http://{proxy}"))
        .build()
        .expect("build fetcher with proxy");

    let url = format!("http://{}/x", upstream);
    let resp = f.request(FetchRequest::new(&url)).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(&resp.body[..], b"upstream-ok");
    assert_eq!(
        connect_count.load(Ordering::SeqCst),
        1,
        "proxy CONNECT not used"
    );
    assert_eq!(
        upstream_count.load(Ordering::SeqCst),
        1,
        "upstream not reached"
    );
}

#[tokio::test]
async fn proxy_off_by_default() {
    // Sanity: without configuring a proxy, the fetcher hits the upstream
    // directly (no CONNECT count, just a request count).
    let (upstream, upstream_count) = spawn_upstream().await;
    let (proxy, connect_count) = spawn_proxy().await;
    let _ = proxy; // proxy is up but not pointed at

    let f = Fetcher::new().expect("build fetcher");
    let url = format!("http://{}/x", upstream);
    let resp = f.request(FetchRequest::new(&url)).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(connect_count.load(Ordering::SeqCst), 0);
    assert_eq!(upstream_count.load(Ordering::SeqCst), 1);
}
