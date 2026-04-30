//! Tests for the extended Fetcher API: arbitrary method, headers, body.
//!
//! TDD-first — this file lands red against the GET-only Fetcher. Once
//! `Fetcher::request(...)` is wired, all four tests go green.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use bouncy_fetch::{FetchRequest, Fetcher};

/// Echo server: responds with status 200 and a JSON body that records the
/// method, all headers it received, and the request body. Useful for
/// asserting on each piece independently.
async fn spawn_echo() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_t = counter.clone();
    let last = Arc::new(Mutex::new(String::new()));
    let _ = last;

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
                        let method = req.method().to_string();
                        let mut headers = serde_json::Map::new();
                        for (k, v) in req.headers().iter() {
                            headers.insert(
                                k.to_string(),
                                serde_json::Value::String(
                                    v.to_str().unwrap_or("<binary>").to_string(),
                                ),
                            );
                        }
                        let body_bytes = req.into_body().collect().await.unwrap().to_bytes();
                        let body_text = String::from_utf8_lossy(&body_bytes).into_owned();
                        let payload = serde_json::json!({
                            "method": method,
                            "headers": headers,
                            "body": body_text,
                        })
                        .to_string();
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .header("content-type", "application/json")
                                .body(Full::new(Bytes::from(payload)))
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
async fn post_with_body_round_trips() {
    let (addr, _) = spawn_echo().await;
    let f = Fetcher::new().expect("build fetcher");
    let url = format!("http://{}/echo", addr);
    let req = FetchRequest::new(&url)
        .method("POST")
        .body_str(r#"{"hello":"world"}"#);
    let resp = f.request(req).await.unwrap();
    assert_eq!(resp.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["method"], "POST");
    assert_eq!(v["body"], r#"{"hello":"world"}"#);
}

#[tokio::test]
async fn custom_headers_are_forwarded() {
    let (addr, _) = spawn_echo().await;
    let f = Fetcher::new().expect("build fetcher");
    let url = format!("http://{}/h", addr);
    let req = FetchRequest::new(&url)
        .header("X-Bouncy-Test", "1")
        .header("Authorization", "Bearer abc");
    let resp = f.request(req).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["headers"]["x-bouncy-test"], "1");
    assert_eq!(v["headers"]["authorization"], "Bearer abc");
}

#[tokio::test]
async fn put_method_sets_method() {
    let (addr, _) = spawn_echo().await;
    let f = Fetcher::new().expect("build fetcher");
    let url = format!("http://{}/x", addr);
    let req = FetchRequest::new(&url).method("PUT").body_str("payload");
    let resp = f.request(req).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["method"], "PUT");
    assert_eq!(v["body"], "payload");
}

#[tokio::test]
async fn get_still_works_via_request_api() {
    let (addr, _) = spawn_echo().await;
    let f = Fetcher::new().expect("build fetcher");
    let url = format!("http://{}/g", addr);
    let resp = f.request(FetchRequest::new(&url)).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["method"], "GET");
    assert_eq!(v["body"], "");
}
