//! End-to-end CDP tests: connect via tokio-tungstenite, send JSON-RPC,
//! assert on the response shape Playwright expects.
//!
//! TDD-red first — this file lands red against the empty bouncy-cdp.

use std::sync::Arc;

use bouncy_cdp::Server;
use bouncy_fetch::Fetcher;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

async fn start_server() -> std::net::SocketAddr {
    let fetcher = Arc::new(Fetcher::new().expect("fetcher"));
    let server = Server::new(fetcher).bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr();
    tokio::spawn(async move {
        let _ = server.serve().await;
    });
    addr
}

async fn ws_connect(
    addr: std::net::SocketAddr,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("ws://{addr}/devtools/browser/page-1");
    let (ws, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws connect");
    ws
}

async fn rpc(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: i64,
    method: &str,
    params: Value,
) -> Value {
    let msg = json!({
        "id": id,
        "method": method,
        "params": params,
    });
    ws.send(Message::Text(msg.to_string().into()))
        .await
        .expect("send rpc");
    // Pull messages until we see one with the matching id (skip events).
    while let Some(m) = ws.next().await {
        let m = m.expect("ws frame");
        if let Message::Text(text) = m {
            let v: Value = serde_json::from_str(&text).expect("server replied with valid json");
            if v.get("id").and_then(Value::as_i64) == Some(id) {
                return v;
            }
            // Otherwise it's an event — ignore and keep reading.
        }
    }
    panic!("ws closed before reply for id={id}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_evaluate_arithmetic() {
    let addr = start_server().await;
    let mut ws = ws_connect(addr).await;

    let resp = rpc(
        &mut ws,
        1,
        "Runtime.evaluate",
        json!({"expression": "1 + 2"}),
    )
    .await;
    assert_eq!(resp["id"], 1);
    let res = &resp["result"]["result"];
    assert_eq!(res["type"], "number", "got: {resp}");
    assert_eq!(res["value"].as_f64(), Some(3.0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_evaluate_string() {
    let addr = start_server().await;
    let mut ws = ws_connect(addr).await;

    let resp = rpc(
        &mut ws,
        7,
        "Runtime.evaluate",
        json!({"expression": "'hi-' + 'there'"}),
    )
    .await;
    assert_eq!(resp["result"]["result"]["type"], "string");
    assert_eq!(resp["result"]["result"]["value"], "hi-there");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_evaluate_returns_exception_on_throw() {
    let addr = start_server().await;
    let mut ws = ws_connect(addr).await;

    let resp = rpc(
        &mut ws,
        2,
        "Runtime.evaluate",
        json!({"expression": "throw new Error('boom')"}),
    )
    .await;
    assert_eq!(resp["id"], 2);
    assert!(
        resp["result"]["exceptionDetails"].is_object(),
        "expected exceptionDetails on throw, got: {resp}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dom_query_selector_and_get_outer_html() {
    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::body::Incoming;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let svc = service_fn(|_req: Request<Incoming>| async move {
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(200)
                            .body(Full::new(Bytes::from_static(
                                b"<html><body><div id='target'>hi</div></body></html>",
                            )))
                            .unwrap(),
                    )
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(s), svc)
                    .await;
            });
        }
    });

    let cdp = start_server().await;
    let mut ws = ws_connect(cdp).await;

    let _ = rpc(
        &mut ws,
        20,
        "Page.navigate",
        json!({"url": format!("http://{upstream_addr}/")}),
    )
    .await;

    let qs = rpc(
        &mut ws,
        21,
        "DOM.querySelector",
        json!({"nodeId": 1, "selector": "#target"}),
    )
    .await;
    let node_id = qs["result"]["nodeId"]
        .as_i64()
        .expect("DOM.querySelector should return a numeric nodeId");
    assert!(node_id != 0, "got: {qs}");

    let outer = rpc(&mut ws, 22, "DOM.getOuterHTML", json!({"nodeId": node_id})).await;
    let html = outer["result"]["outerHTML"]
        .as_str()
        .expect("DOM.getOuterHTML should return a string");
    assert!(html.contains("<div"), "got: {html}");
    assert!(html.contains("hi"), "got: {html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dom_query_selector_returns_zero_for_no_match() {
    let cdp = start_server().await;
    let mut ws = ws_connect(cdp).await;
    let qs = rpc(
        &mut ws,
        30,
        "DOM.querySelector",
        json!({"nodeId": 1, "selector": ".does-not-exist"}),
    )
    .await;
    // Real CDP returns nodeId: 0 for no match.
    assert_eq!(qs["result"]["nodeId"].as_i64(), Some(0), "got: {qs}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn network_set_extra_http_headers_is_applied_on_navigate() {
    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::body::Incoming;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let saw_header = Arc::new(AtomicBool::new(false));
    let h = saw_header.clone();
    tokio::spawn(async move {
        loop {
            let (s, _) = listener.accept().await.unwrap();
            let h2 = h.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let h3 = h2.clone();
                    async move {
                        if let Some(v) = req.headers().get("x-bouncy-extra") {
                            if v == "yes" {
                                h3.store(true, Ordering::SeqCst);
                            }
                        }
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .body(Full::new(Bytes::from_static(
                                    b"<html><body>ok</body></html>",
                                )))
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

    let cdp = start_server().await;
    let mut ws = ws_connect(cdp).await;

    let _ = rpc(
        &mut ws,
        40,
        "Network.setExtraHTTPHeaders",
        json!({"headers": {"X-Bouncy-Extra": "yes"}}),
    )
    .await;

    let _ = rpc(
        &mut ws,
        41,
        "Page.navigate",
        json!({"url": format!("http://{upstream_addr}/")}),
    )
    .await;

    assert!(
        saw_header.load(Ordering::SeqCst),
        "upstream did not receive the extra header"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_dispatch_mouse_event_acks() {
    // We don't have layout/hit-testing, so dispatchMouseEvent can't
    // actually click a target by coordinates — but Puppeteer's click
    // flow expects a successful ack, otherwise it bails out before
    // even starting. So this just asserts the protocol shape.
    let cdp = start_server().await;
    let mut ws = ws_connect(cdp).await;
    let r = rpc(
        &mut ws,
        50,
        "Input.dispatchMouseEvent",
        json!({"type": "mousePressed", "x": 1, "y": 1, "button": "left"}),
    )
    .await;
    assert_eq!(r["id"], 50);
    assert!(
        r.get("error").is_none(),
        "Input.dispatchMouseEvent should not error: {r}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn page_navigate_loads_url() {
    // Spin a tiny upstream that says "navigated-ok" so we can assert
    // Page.navigate actually fetched it (via Runtime.evaluate after).
    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::body::Incoming;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let svc = service_fn(|_req: Request<Incoming>| async move {
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(200)
                            .body(Full::new(Bytes::from_static(
                                b"<html><head><title>Navigated</title></head><body><h1 id='t'>navigated-ok</h1></body></html>",
                            )))
                            .unwrap(),
                    )
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(s), svc)
                    .await;
            });
        }
    });

    let cdp = start_server().await;
    let mut ws = ws_connect(cdp).await;

    let nav = rpc(
        &mut ws,
        10,
        "Page.navigate",
        json!({ "url": format!("http://{upstream_addr}/") }),
    )
    .await;
    assert!(nav["result"]["frameId"].is_string(), "got: {nav}");

    let title = rpc(
        &mut ws,
        11,
        "Runtime.evaluate",
        json!({"expression": "document.title"}),
    )
    .await;
    assert_eq!(title["result"]["result"]["value"], "Navigated");

    let h1 = rpc(
        &mut ws,
        12,
        "Runtime.evaluate",
        json!({"expression": "document.getElementById('t').textContent"}),
    )
    .await;
    assert_eq!(h1["result"]["result"]["value"], "navigated-ok");
}
