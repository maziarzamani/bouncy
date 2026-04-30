//! Per-request timeout test: a server that holds the response, a fetch
//! configured with a tight timeout, and we assert the fetch errors out
//! within the budget rather than waiting forever.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bouncy_fetch::{FetchRequest, Fetcher};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

async fn spawn_slow_server(delay: Duration) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let svc = service_fn(move |_req: Request<Incoming>| async move {
                    tokio::time::sleep(delay).await;
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(200)
                            .body(Full::new(Bytes::from_static(b"slow")))
                            .unwrap(),
                    )
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(s), svc)
                    .await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn request_timeout_fires() {
    let addr = spawn_slow_server(Duration::from_secs(5)).await;
    let f = Fetcher::builder()
        .request_timeout(Duration::from_millis(100))
        .build()
        .unwrap();
    let url = format!("http://{}/x", addr);
    let started = Instant::now();
    let err = f.request(FetchRequest::new(&url)).await.unwrap_err();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "took {elapsed:?} — timeout did not fire"
    );
    assert!(
        format!("{err}").to_lowercase().contains("timeout")
            || format!("{err}").to_lowercase().contains("timed out"),
        "expected timeout error, got: {err}"
    );
}

#[tokio::test]
async fn no_timeout_set_completes_normally() {
    let addr = spawn_slow_server(Duration::from_millis(50)).await;
    let f = Fetcher::new().unwrap();
    let url = format!("http://{}/x", addr);
    let resp = f.request(FetchRequest::new(&url)).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(&resp.body[..], b"slow");
}
