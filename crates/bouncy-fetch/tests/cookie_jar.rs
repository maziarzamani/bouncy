//! Cookie jar tests at the Fetcher level — the jar lives in the Fetcher
//! so static-path callers (bouncy scrape) get cross-request cookie
//! persistence too, not just JS-path bridge calls.

use std::convert::Infallible;
use std::net::SocketAddr;

use bouncy_fetch::{CookieJar, FetchRequest, Fetcher};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

async fn spawn_login() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let svc = service_fn(|req: Request<Incoming>| async move {
                    let path = req.uri().path().to_string();
                    let cookie = req
                        .headers()
                        .get("cookie")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    let (status, set_cookie, body): (u16, Option<&str>, Bytes) = match path.as_str()
                    {
                        "/login" => (200, Some("token=abc; Path=/"), Bytes::from_static(b"in")),
                        "/me" => {
                            if cookie.contains("token=abc") {
                                (200, None, Bytes::from_static(b"ok"))
                            } else {
                                (401, None, Bytes::from_static(b"no"))
                            }
                        }
                        _ => (404, None, Bytes::from_static(b"")),
                    };
                    let mut r = Response::builder().status(status);
                    if let Some(sc) = set_cookie {
                        r = r.header("Set-Cookie", sc);
                    }
                    Ok::<_, Infallible>(r.body(Full::new(body)).unwrap())
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
async fn cookies_replay_within_a_fetcher() {
    let addr = spawn_login().await;
    let jar = CookieJar::new();
    let f = Fetcher::builder().cookie_jar(jar.clone()).build().unwrap();

    let r1 = f
        .request(FetchRequest::new(format!("http://{addr}/login")))
        .await
        .unwrap();
    assert_eq!(r1.status, 200);

    let r2 = f
        .request(FetchRequest::new(format!("http://{addr}/me")))
        .await
        .unwrap();
    assert_eq!(r2.status, 200, "/me did not see the cookie");
    assert_eq!(&r2.body[..], b"ok");
}

#[tokio::test]
async fn jar_round_trips_to_json() {
    let jar = CookieJar::new();
    jar.set("https://x.test", "session", "abc123");
    jar.set("https://x.test", "csrf", "z9");
    jar.set("https://other.test", "n", "v");

    let json = jar.to_json();
    let restored = CookieJar::from_json(&json).expect("parse jar");
    assert_eq!(
        restored.get("https://x.test", "session").as_deref(),
        Some("abc123")
    );
    assert_eq!(
        restored.get("https://x.test", "csrf").as_deref(),
        Some("z9")
    );
    assert_eq!(
        restored.get("https://other.test", "n").as_deref(),
        Some("v")
    );
    assert_eq!(restored.get("https://x.test", "missing"), None);
}

#[tokio::test]
async fn jar_can_be_pre_populated() {
    let addr = spawn_login().await;
    let jar = CookieJar::new();
    // Pre-populate as if loaded from disk.
    jar.set(&format!("http://{addr}"), "token", "abc");
    let f = Fetcher::builder().cookie_jar(jar.clone()).build().unwrap();

    let r = f
        .request(FetchRequest::new(format!("http://{addr}/me")))
        .await
        .unwrap();
    assert_eq!(r.status, 200, "pre-loaded jar didn't replay cookie");
    assert_eq!(&r.body[..], b"ok");
}
