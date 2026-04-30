//! Tests for cookie persistence within a Runtime session.
//!
//! TDD: spin up a tiny `/login` + `/me` flow. `/login` issues Set-Cookie;
//! `/me` requires that exact cookie or fails. The Runtime is expected to
//! parse `Set-Cookie` from each response and replay it on subsequent
//! requests to the same origin — so the second fetch gets the cookie even
//! though JS never explicitly forwarded it.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bouncy_fetch::Fetcher;
use bouncy_js::Runtime;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::Request;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

async fn spawn_login_server() -> SocketAddr {
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
                    let path = req.uri().path().to_string();
                    let cookie = req
                        .headers()
                        .get("cookie")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    let (status, set_cookie, body): (u16, Option<&str>, Bytes) = match path.as_str()
                    {
                        "/login" => (
                            200,
                            Some("token=abc123; Path=/"),
                            Bytes::from_static(b"logged-in"),
                        ),
                        "/me" => {
                            if cookie.contains("token=abc123") {
                                (200, None, Bytes::from_static(b"ok"))
                            } else {
                                (401, None, Bytes::from_static(b"no-cookie"))
                            }
                        }
                        _ => (404, None, Bytes::from_static(b"")),
                    };
                    let mut resp = hyper::Response::builder().status(status);
                    if let Some(sc) = set_cookie {
                        resp = resp.header("Set-Cookie", sc);
                    }
                    Ok::<_, Infallible>(resp.body(Full::new(body)).unwrap())
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    addr
}

fn build_runtime() -> Runtime {
    Runtime::new(
        tokio::runtime::Handle::current(),
        Arc::new(Fetcher::new().expect("fetcher")),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_cookie_replays_on_next_request() {
    let addr = spawn_login_server().await;
    let mut rt = build_runtime();
    rt.load("<html><body></body></html>", &format!("http://{addr}/"))
        .unwrap();

    rt.eval(
        r#"
        let __login_status, __me_status, __me_body;
        (async () => {
            const r1 = await fetch('/login');
            __login_status = r1.status;
            const r2 = await fetch('/me');
            __me_status = r2.status;
            __me_body = await r2.text();
        })();
        "#,
    )
    .unwrap();

    assert_eq!(rt.eval("__login_status").unwrap(), "200");
    assert_eq!(
        rt.eval("__me_status").unwrap(),
        "200",
        "/me did not see the cookie set by /login"
    );
    assert_eq!(rt.eval("__me_body").unwrap(), "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cookies_scoped_to_origin() {
    // Fresh Runtime per origin: cookies set against host A don't bleed
    // into requests against host B (different port = different origin).
    let a = spawn_login_server().await;
    let b = spawn_login_server().await;
    let mut rt = build_runtime();
    rt.load("<html><body></body></html>", &format!("http://{a}/"))
        .unwrap();

    rt.eval(&format!(
        r#"
        let __ok, __cross;
        (async () => {{
            await fetch('http://{a}/login');
            const r_a = await fetch('http://{a}/me');
            __ok = r_a.status;
            const r_b = await fetch('http://{b}/me');
            __cross = r_b.status;
        }})();
        "#
    ))
    .unwrap();

    assert_eq!(rt.eval("__ok").unwrap(), "200", "same-origin replay broken");
    assert_eq!(
        rt.eval("__cross").unwrap(),
        "401",
        "cookie leaked across origin boundary"
    );
}
