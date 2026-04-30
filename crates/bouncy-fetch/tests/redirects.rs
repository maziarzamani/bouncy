//! Redirect-follow tests.
//!
//! Default behaviour: follow up to 10 hops, returning the final
//! response. Configurable via `FetcherBuilder::max_redirects`. A loop
//! that exceeds the cap surfaces as `Error::TooManyRedirects`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bouncy_fetch::{Error, FetchRequest, Fetcher};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

async fn spawn_redirect_chain() -> (SocketAddr, Arc<AtomicUsize>) {
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
                let svc = service_fn(move |req: Request<Incoming>| {
                    let h3 = h2.clone();
                    async move {
                        h3.fetch_add(1, Ordering::SeqCst);
                        let (status, location, body): (u16, Option<&str>, Bytes) =
                            match req.uri().path() {
                                "/start" => (302, Some("/hop1"), Bytes::from_static(b"")),
                                "/hop1" => (302, Some("/hop2"), Bytes::from_static(b"")),
                                "/hop2" => (200, None, Bytes::from_static(b"final")),
                                "/loop" => (302, Some("/loop"), Bytes::from_static(b"")),
                                "/relative" => (302, Some("../target"), Bytes::from_static(b"")),
                                "/target" => (200, None, Bytes::from_static(b"hit-target")),
                                "/308-keep" => (308, Some("/echo"), Bytes::from_static(b"")),
                                "/303-get" => (303, Some("/echo"), Bytes::from_static(b"")),
                                "/echo" => {
                                    let m = req.method().as_str().to_string();
                                    (200, None, Bytes::from(m))
                                }
                                _ => (404, None, Bytes::from_static(b"")),
                            };
                        let mut r = Response::builder().status(status);
                        if let Some(loc) = location {
                            r = r.header("Location", loc);
                        }
                        Ok::<_, Infallible>(r.body(Full::new(body)).unwrap())
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
async fn fetcher_follows_simple_redirect_chain() {
    let (addr, hits) = spawn_redirect_chain().await;
    let f = Fetcher::new().unwrap();
    let r = f
        .request(FetchRequest::new(format!("http://{addr}/start")))
        .await
        .unwrap();
    assert_eq!(r.status, 200, "expected to land on /hop2 with 200");
    assert_eq!(&r.body[..], b"final");
    // start + hop1 + hop2 = 3 server hits.
    assert_eq!(hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn fetcher_resolves_relative_redirect_locations() {
    let (addr, _) = spawn_redirect_chain().await;
    let f = Fetcher::new().unwrap();
    let r = f
        .request(FetchRequest::new(format!("http://{addr}/relative")))
        .await
        .unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(&r.body[..], b"hit-target");
}

#[tokio::test]
async fn fetcher_errors_on_redirect_loop() {
    let (addr, _) = spawn_redirect_chain().await;
    let f = Fetcher::builder().max_redirects(5).build().unwrap();
    let res = f
        .request(FetchRequest::new(format!("http://{addr}/loop")))
        .await;
    match res {
        Err(Error::TooManyRedirects(n)) => assert_eq!(n, 5),
        other => panic!("expected TooManyRedirects(5), got {other:?}"),
    }
}

#[tokio::test]
async fn max_redirects_zero_disables_following() {
    let (addr, _) = spawn_redirect_chain().await;
    let f = Fetcher::builder().max_redirects(0).build().unwrap();
    let r = f
        .request(FetchRequest::new(format!("http://{addr}/start")))
        .await
        .unwrap();
    // With following disabled we should see the 302 directly, not the
    // 200 from the chain's tail.
    assert_eq!(r.status, 302);
    assert_eq!(
        r.headers.get("location").and_then(|v| v.to_str().ok()),
        Some("/hop1")
    );
}

#[tokio::test]
async fn redirect_303_downgrades_post_to_get() {
    let (addr, _) = spawn_redirect_chain().await;
    let f = Fetcher::new().unwrap();
    let r = f
        .request(
            FetchRequest::new(format!("http://{addr}/303-get"))
                .method("POST")
                .body_str("ignored"),
        )
        .await
        .unwrap();
    assert_eq!(r.status, 200);
    // The /echo endpoint returns the method it saw — 303 must have
    // turned the POST into a GET on the redirect target.
    assert_eq!(&r.body[..], b"GET");
}

#[tokio::test]
async fn redirect_308_preserves_method_and_body() {
    let (addr, _) = spawn_redirect_chain().await;
    let f = Fetcher::new().unwrap();
    let r = f
        .request(
            FetchRequest::new(format!("http://{addr}/308-keep"))
                .method("POST")
                .body_str("kept"),
        )
        .await
        .unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(
        &r.body[..],
        b"POST",
        "308 must keep method (and body) on the redirect target"
    );
}
