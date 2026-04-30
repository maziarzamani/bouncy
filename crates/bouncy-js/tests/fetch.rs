//! Tests for fetch() / XMLHttpRequest polyfills going through the
//! __bouncy_sync_fetch bridge against an in-process hyper server.

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

async fn spawn_fixture_server() -> SocketAddr {
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
                    let body = match req.uri().path() {
                        "/api/posts.json" => Bytes::from_static(
                            br#"[{"id":1,"title":"First"},{"id":2,"title":"Second"}]"#,
                        ),
                        "/text" => Bytes::from_static(b"hello"),
                        _ => Bytes::from_static(b"{}"),
                    };
                    Ok::<_, Infallible>(
                        hyper::Response::builder()
                            .status(200)
                            .header("content-type", "application/json")
                            .body(Full::new(body))
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

fn build_runtime() -> Runtime {
    let handle = tokio::runtime::Handle::current();
    let fetcher = Arc::new(Fetcher::new().expect("build fetcher"));
    Runtime::new(handle, fetcher)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_returns_text() {
    let addr = spawn_fixture_server().await;
    let mut rt = build_runtime();
    rt.load("<html><body></body></html>", &format!("http://{addr}/"))
        .unwrap();

    let v = rt
        .eval(
            r#"
            (async () => {
                const r = await fetch('/text');
                if (!r.ok) throw new Error('http ' + r.status);
                return await r.text();
            })()
        "#,
        )
        .unwrap();
    // The script returns a Promise; eval coerces to string ("[object Promise]").
    // Pull the value out via a global instead.
    rt.eval(
        r#"
        let __out;
        (async () => {
            const r = await fetch('/text');
            __out = await r.text();
        })();
    "#,
    )
    .unwrap();
    // Tiny delay to allow the resolved-Promise microtask to run.
    let _ = v;
    let result = rt.eval("__out").unwrap();
    assert_eq!(result, "hello");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_returns_json() {
    let addr = spawn_fixture_server().await;
    let mut rt = build_runtime();
    rt.load("<html><body></body></html>", &format!("http://{addr}/"))
        .unwrap();

    rt.eval(
        r#"
        let __posts;
        (async () => {
            const r = await fetch('/api/posts.json');
            const j = await r.json();
            __posts = j;
        })();
    "#,
    )
    .unwrap();
    let len = rt.eval("__posts.length").unwrap();
    assert_eq!(len, "2");
    let title = rt.eval("__posts[1].title").unwrap();
    assert_eq!(title, "Second");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xmlhttprequest_synchronous_onload() {
    let addr = spawn_fixture_server().await;
    let mut rt = build_runtime();
    rt.load("<html><body></body></html>", &format!("http://{addr}/"))
        .unwrap();

    let v = rt
        .eval(
            r#"
        (function () {
            const xhr = new XMLHttpRequest();
            let ok = false, status = 0;
            xhr.open('GET', '/api/posts.json');
            xhr.onload = () => { ok = true; status = xhr.status; };
            xhr.send();
            return ok && status === 200 ? xhr.responseText.length : -1;
        })();
    "#,
        )
        .unwrap();
    assert_eq!(v, "52"); // length of the posts.json bytes
}
