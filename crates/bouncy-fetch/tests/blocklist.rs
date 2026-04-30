//! Tracker-blocklist tests. We need TWO behaviours:
//!   1. Pure host-match logic (unit) — exact + subdomain, ignore the rest.
//!   2. End-to-end: a blocked URL short-circuits in `Fetcher::request`
//!      without opening a TCP connection to the upstream server.
//!
//! Together they prove that ad/analytics requests cost ~zero latency and
//! never leak the visitor's IP to the tracker.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bouncy_fetch::{FetchRequest, Fetcher, TrackerBlocklist};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

#[test]
fn blocklist_matches_exact_host() {
    let bl = TrackerBlocklist::from_hosts(["google-analytics.com"]);
    assert!(bl.blocks("https://google-analytics.com/collect"));
}

#[test]
fn blocklist_matches_subdomains() {
    let bl = TrackerBlocklist::from_hosts(["google-analytics.com"]);
    assert!(bl.blocks("https://www.google-analytics.com/collect"));
    assert!(bl.blocks("https://ssl.google-analytics.com/collect"));
}

#[test]
fn blocklist_does_not_match_unrelated_hosts() {
    let bl = TrackerBlocklist::from_hosts(["google-analytics.com"]);
    assert!(!bl.blocks("https://example.com/"));
    // Substring-but-not-suffix should not match.
    assert!(!bl.blocks("https://notgoogle-analytics.com/"));
}

#[test]
fn default_set_blocks_known_trackers() {
    let bl = TrackerBlocklist::default_set();
    for u in [
        "https://www.google-analytics.com/collect",
        "https://googletagmanager.com/gtm.js",
        "https://doubleclick.net/x",
        "https://connect.facebook.net/en_US/fbevents.js",
    ] {
        assert!(bl.blocks(u), "expected default set to block {u}");
    }
    assert!(!bl.blocks("https://example.com/"));
}

async fn spawn_counted_server() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    tokio::spawn(async move {
        loop {
            let (s, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let h2 = h.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |_req: Request<Incoming>| {
                    let h3 = h2.clone();
                    async move {
                        h3.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .body(Full::new(Bytes::from_static(b"hit")))
                                .unwrap(),
                        )
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(s), svc)
                    .await;
            });
        }
    });
    (addr, hits)
}

#[tokio::test]
async fn blocked_requests_short_circuit_without_hitting_network() {
    let (addr, hits) = spawn_counted_server().await;
    // Block by exact host:port — works for the test loopback server.
    let bl = TrackerBlocklist::from_hosts([format!("{addr}")]);
    let f = Fetcher::builder().tracker_blocklist(bl).build().unwrap();

    let r = f
        .request(FetchRequest::new(format!("http://{addr}/track")))
        .await
        .unwrap();
    assert_eq!(r.status, 204, "blocked request should return 204");
    assert!(r.body.is_empty(), "blocked request body should be empty");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "server should not have been hit at all"
    );
}

#[tokio::test]
async fn unblocked_requests_pass_through_to_upstream() {
    let (addr, hits) = spawn_counted_server().await;
    // Block a different host so this server's URL passes through.
    let bl = TrackerBlocklist::from_hosts(["does-not-match.test"]);
    let f = Fetcher::builder().tracker_blocklist(bl).build().unwrap();

    let r = f
        .request(FetchRequest::new(format!("http://{addr}/x")))
        .await
        .unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(&r.body[..], b"hit");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}
