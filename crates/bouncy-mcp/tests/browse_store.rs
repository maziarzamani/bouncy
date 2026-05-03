//! Integration tests for `BrowseStore` — the server-side map of
//! active `BrowseSession`s. Spins up a small `tiny_http` fixture so
//! tests exercise real session creation (V8 + cookie jar + DOM)
//! through the store interface.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use bouncy_browse::BrowseOpts;
use bouncy_mcp::browse_store::{BrowseStore, StoreError};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

const PAGE: &str =
    "<!doctype html><html><head><title>S</title></head><body><h1>x</h1></body></html>";

async fn spawn() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let svc = service_fn(|_req: Request<Incoming>| async move {
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(200)
                            .header("content-type", "text/html")
                            .body(Full::new(Bytes::from_static(PAGE.as_bytes())))
                            .unwrap(),
                    )
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_then_touch_returns_same_session_handle() {
    let store = BrowseStore::default();
    let addr = spawn().await;
    let (id, snap) = store
        .open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    assert_eq!(snap.title, "S");
    assert_eq!(store.len(), 1);
    let s = store.touch(&id).expect("session present");
    // Drive a click via the touched handle to confirm it's a real,
    // working session — no panic, returns a snapshot.
    let _ = s.snapshot().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn touch_unknown_id_returns_not_found() {
    let store = BrowseStore::default();
    let err = store.touch("ghost").unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_removes_session_and_subsequent_touch_fails() {
    let store = BrowseStore::default();
    let addr = spawn().await;
    let (id, _) = store
        .open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    assert!(store.close(&id), "close on existing id should return true");
    assert!(
        !store.close(&id),
        "close on already-closed id should return false (idempotent)"
    );
    assert!(matches!(store.touch(&id), Err(StoreError::NotFound(_))));
    assert_eq!(store.len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cap_rejects_open_past_max_sessions() {
    // Cap of 2 — the third open should fail with AtCapacity.
    let store = BrowseStore::new(2, Duration::from_secs(60));
    let addr = spawn().await;
    let (_id1, _) = store
        .open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    let (_id2, _) = store
        .open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    let err = store
        .open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::AtCapacity { cap: 2 }));
    assert_eq!(store.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reap_expired_drops_idle_sessions() {
    // Idle timeout 50 ms — open one, sleep past it, reap, assert gone.
    let store = BrowseStore::new(20, Duration::from_millis(50));
    let addr = spawn().await;
    let (id, _) = store
        .open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    assert_eq!(store.len(), 1);
    tokio::time::sleep(Duration::from_millis(75)).await;
    store.reap_expired();
    assert_eq!(
        store.len(),
        0,
        "reaper should have removed the idle session"
    );
    assert!(matches!(store.touch(&id), Err(StoreError::NotFound(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn touch_resets_idle_timer() {
    // Open, sleep almost to the edge, touch (resets timer), sleep
    // again to past the original deadline. The session should still be
    // alive because the touch reset the clock.
    let store = BrowseStore::new(20, Duration::from_millis(100));
    let addr = spawn().await;
    let (id, _) = store
        .open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(70)).await;
    let _ = store
        .touch(&id)
        .expect("touch should succeed before timeout");
    tokio::time::sleep(Duration::from_millis(70)).await;
    store.reap_expired();
    assert_eq!(
        store.len(),
        1,
        "session should still be alive after touch + sleep < timeout"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_sessions_are_isolated() {
    // Two sessions on the same store should not see each other's state.
    // Drive each one; assert the snapshot URL matches what we opened.
    let store = BrowseStore::default();
    let a = spawn().await;
    let b = spawn().await;
    let (id_a, snap_a) = store
        .open(&format!("http://{a}/"), BrowseOpts::default())
        .await
        .unwrap();
    let (id_b, snap_b) = store
        .open(&format!("http://{b}/"), BrowseOpts::default())
        .await
        .unwrap();
    assert_eq!(snap_a.url, format!("http://{a}/"));
    assert_eq!(snap_b.url, format!("http://{b}/"));
    assert_ne!(id_a, id_b);
    let s_a = store.touch(&id_a).unwrap();
    let s_b = store.touch(&id_b).unwrap();
    let snap_a2 = s_a.snapshot().await.unwrap();
    let snap_b2 = s_b.snapshot().await.unwrap();
    assert_eq!(snap_a2.url, format!("http://{a}/"));
    assert_eq!(snap_b2.url, format!("http://{b}/"));
}
