//! Tests for Fetcher's User-Agent handling.
//!
//! Contract:
//! - Default UA is `bouncy/<version> (+homepage)` — sourced from
//!   `env!("CARGO_PKG_VERSION")` so the version stays in lockstep with
//!   the crate.
//! - `Fetcher::builder().user_agent(s)` overrides the default.
//! - A per-request `User-Agent` header overrides the Fetcher default
//!   (last-write-wins, matching how `Accept-Encoding` works today).

use std::convert::Infallible;
use std::net::SocketAddr;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use bouncy_fetch::{FetchRequest, Fetcher};

/// Echo server: returns the inbound `User-Agent` header in the body so
/// each test can assert exactly what the Fetcher sent.
async fn spawn_ua_echo() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let svc = service_fn(|req: Request<Incoming>| async move {
                    let ua = req
                        .headers()
                        .get(hyper::header::USER_AGENT)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<missing>")
                        .to_string();
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(200)
                            .body(Full::new(Bytes::from(ua)))
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

#[tokio::test]
async fn default_user_agent_identifies_as_bouncy() {
    let addr = spawn_ua_echo().await;
    let f = Fetcher::new().expect("build fetcher");
    let resp = f.get(&format!("http://{}/", addr)).await.unwrap();
    assert_eq!(resp.status, 200);
    let body = String::from_utf8_lossy(&resp.body);
    // Default UA shape: `bouncy/<version> (+<url>)`. We don't assert the
    // exact homepage string (it can move), but it must start with
    // `bouncy/` and contain the crate version.
    let expected_prefix = format!("bouncy/{}", env!("CARGO_PKG_VERSION"));
    assert!(
        body.starts_with(&expected_prefix),
        "expected default UA to start with `{}`, got: {}",
        expected_prefix,
        body
    );
}

#[tokio::test]
async fn builder_user_agent_overrides_default() {
    let addr = spawn_ua_echo().await;
    let f = Fetcher::builder()
        .user_agent("my-bot/1.0 (+contact@example.com)")
        .build()
        .expect("build fetcher");
    let resp = f.get(&format!("http://{}/", addr)).await.unwrap();
    let body = String::from_utf8_lossy(&resp.body);
    assert_eq!(body, "my-bot/1.0 (+contact@example.com)");
}

#[tokio::test]
async fn per_request_user_agent_overrides_builder_value() {
    let addr = spawn_ua_echo().await;
    let f = Fetcher::builder()
        .user_agent("default-bot/1.0")
        .build()
        .expect("build fetcher");
    let req =
        FetchRequest::new(format!("http://{addr}/")).header("User-Agent", "per-request-bot/9.9");
    let resp = f.request(req).await.unwrap();
    let body = String::from_utf8_lossy(&resp.body);
    assert_eq!(body, "per-request-bot/9.9");
}

#[tokio::test]
async fn per_request_user_agent_case_insensitive_override() {
    // Real-world clients send the header with various casings — make
    // sure our case-insensitive override check actually works.
    let addr = spawn_ua_echo().await;
    let f = Fetcher::builder()
        .user_agent("default-bot/1.0")
        .build()
        .expect("build fetcher");
    let req =
        FetchRequest::new(format!("http://{addr}/")).header("user-agent", "lowercase-bot/2.2");
    let resp = f.request(req).await.unwrap();
    let body = String::from_utf8_lossy(&resp.body);
    assert_eq!(body, "lowercase-bot/2.2");
}

#[tokio::test]
async fn empty_user_agent_string_is_rejected_at_build_time() {
    // Empty UA is meaningless and most servers reject it. Make this
    // a build-time error rather than a silent identity-as-nothing.
    let result = Fetcher::builder().user_agent("").build();
    assert!(
        result.is_err(),
        "expected empty user_agent to fail at build, got Ok"
    );
}
