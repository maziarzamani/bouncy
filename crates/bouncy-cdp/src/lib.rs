//! Minimal Chrome DevTools Protocol over WebSocket.
//!
//! `bouncy serve --port 9222` exposes a tiny CDP surface: `Runtime.evaluate`,
//! `Page.navigate`, and the no-op handshake methods Playwright fires
//! on connect (`Page.enable`, `Runtime.enable`, `DOM.enable`,
//! `Target.setDiscoverTargets`). Each WebSocket session owns a fresh
//! `bouncy-js` Runtime — there's no multi-tab state machine in this
//! version; one WS = one page.

use std::net::SocketAddr;
use std::sync::Arc;

use bouncy_fetch::Fetcher;
use bouncy_js::Runtime;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("ws: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
}

pub struct Server {
    fetcher: Arc<Fetcher>,
    listener: Option<TcpListener>,
}

impl Server {
    pub fn new(fetcher: Arc<Fetcher>) -> Self {
        Self {
            fetcher,
            listener: None,
        }
    }

    pub async fn bind(mut self, addr: &str) -> Result<Self, Error> {
        self.listener = Some(TcpListener::bind(addr).await?);
        Ok(self)
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.listener
            .as_ref()
            .expect("call bind() before local_addr()")
            .local_addr()
            .expect("listener has a local_addr")
    }

    pub async fn serve(self) -> Result<(), Error> {
        let listener = self
            .listener
            .expect("Server::serve called before Server::bind");
        let fetcher = self.fetcher;
        loop {
            let (stream, _) = listener.accept().await?;
            let fetcher = fetcher.clone();
            // V8's OwnedIsolate is !Send, so the per-session work can't
            // run on a tokio worker (tokio::spawn requires Send).
            // Park each session on a dedicated blocking thread that owns
            // the Runtime and uses the parent multi_thread runtime's
            // handle to drive async ops via block_on.
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                let inner_handle = handle.clone();
                inner_handle.block_on(async move {
                    if let Err(e) = handle_connection(stream, fetcher, handle).await {
                        tracing::warn!("cdp session ended: {e:?}");
                    }
                });
            });
        }
    }
}

/// Per-WebSocket-session CDP state. One session = one page; we don't
/// model multi-tab here.
struct Session {
    runtime: Runtime,
    /// Headers added to every Page.navigate fetch via
    /// `Network.setExtraHTTPHeaders`. Replaced (not merged) on each
    /// `setExtraHTTPHeaders` call to match Chrome's behaviour.
    extra_headers: Vec<(String, String)>,
}

async fn handle_connection(
    stream: TcpStream,
    fetcher: Arc<Fetcher>,
    rt_handle: tokio::runtime::Handle,
) -> Result<(), Error> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut stream) = ws.split();

    let mut session = Session {
        runtime: Runtime::new(rt_handle, fetcher),
        extra_headers: Vec::new(),
    };
    // Start with an empty document so Runtime.evaluate works before a
    // Page.navigate. Playwright expects to be able to attach + evaluate
    // before any navigation.
    let _ = session
        .runtime
        .load("<html><body></body></html>", "about:blank");

    while let Some(frame) = stream.next().await {
        let msg = match frame {
            Ok(Message::Text(t)) => t.to_string(),
            Ok(Message::Binary(_)) => continue,
            Ok(Message::Ping(p)) => {
                sink.send(Message::Pong(p)).await?;
                continue;
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => continue,
        };
        let req: RpcRequest = match serde_json::from_str(&msg) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let response = dispatch(&mut session, req).await;
        sink.send(Message::Text(response.to_string().into()))
            .await?;
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct RpcRequest {
    id: i64,
    method: String,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

async fn dispatch(session: &mut Session, req: RpcRequest) -> Value {
    let id = req.id;
    let session_id = req.session_id.clone();
    let result = match req.method.as_str() {
        // Handshake / no-op acks Playwright fires on connect.
        "Page.enable"
        | "Runtime.enable"
        | "DOM.enable"
        | "Network.enable"
        | "Target.setDiscoverTargets"
        | "Target.setAutoAttach" => Ok(json!({})),

        "Browser.getVersion" => Ok(json!({
            "protocolVersion": "1.3",
            "product": "bouncy/0.0.0",
            "revision": "@",
            "userAgent": "bouncy/0.0.0",
            "jsVersion": "v8",
        })),

        "Runtime.evaluate" => runtime_evaluate(&mut session.runtime, &req.params),

        "Page.navigate" => page_navigate(session, &req.params).await,

        "DOM.querySelector" => dom_query_selector(&mut session.runtime, &req.params),
        "DOM.getOuterHTML" => dom_get_outer_html(&mut session.runtime, &req.params),

        "Network.setExtraHTTPHeaders" => network_set_extra_http_headers(session, &req.params),

        // Without a layout / hit-tester there's nothing meaningful to do
        // with mouse coordinates. Ack so Playwright's click flow doesn't
        // bail out before it gets to its own retry / fallback path.
        "Input.dispatchMouseEvent" | "Input.dispatchKeyEvent" => Ok(json!({})),

        // Best-effort acks for things we don't fully model — return an
        // empty object instead of a JSON-RPC error so puppeteer-core
        // doesn't bail out.
        _ => Ok(json!({})),
    };

    let mut out = match result {
        Ok(value) => json!({"id": id, "result": value}),
        Err(message) => json!({
            "id": id,
            "error": {"code": -32000, "message": message},
        }),
    };
    if let Some(s) = session_id {
        out["sessionId"] = Value::String(s);
    }
    out
}

fn dom_query_selector(runtime: &mut Runtime, params: &Value) -> Result<Value, String> {
    let selector = params
        .get("selector")
        .and_then(Value::as_str)
        .ok_or_else(|| "DOM.querySelector: missing selector".to_string())?;
    // Match Chrome: nodeId = 0 means "no match".
    let nid = runtime.query_selector(selector).unwrap_or(0);
    Ok(json!({"nodeId": nid}))
}

fn dom_get_outer_html(runtime: &mut Runtime, params: &Value) -> Result<Value, String> {
    let nid = params
        .get("nodeId")
        .and_then(Value::as_u64)
        .ok_or_else(|| "DOM.getOuterHTML: missing nodeId".to_string())?;
    let html = runtime
        .outer_html(nid as u32)
        .ok_or_else(|| format!("DOM.getOuterHTML: unknown nodeId {nid}"))?;
    Ok(json!({"outerHTML": html}))
}

fn network_set_extra_http_headers(session: &mut Session, params: &Value) -> Result<Value, String> {
    let headers = params
        .get("headers")
        .and_then(Value::as_object)
        .ok_or_else(|| "Network.setExtraHTTPHeaders: missing headers".to_string())?;
    let mut out = Vec::with_capacity(headers.len());
    for (k, v) in headers {
        if let Some(s) = v.as_str() {
            out.push((k.clone(), s.to_string()));
        }
    }
    session.extra_headers = out;
    Ok(json!({}))
}

fn runtime_evaluate(runtime: &mut Runtime, params: &Value) -> Result<Value, String> {
    let expression = params
        .get("expression")
        .and_then(Value::as_str)
        .unwrap_or("");
    match runtime.eval(expression) {
        Ok(s) => Ok(json!({"result": classify_result(&s)})),
        Err(e) => Ok(json!({
            "result": {"type": "undefined"},
            "exceptionDetails": {
                "exceptionId": 1,
                "text": e.to_string(),
            }
        })),
    }
}

/// `Runtime.evaluate` returns a `RemoteObject` describing the value's
/// type. We don't have type info from `Runtime::eval` (it stringifies
/// before returning), so heuristically classify: numeric strings →
/// number, "true"/"false" → boolean, "null"/"undefined" → those, else
/// string. Good enough for puppeteer's `await page.evaluate('1+1')`.
fn classify_result(s: &str) -> Value {
    if s == "null" {
        return json!({"type": "object", "subtype": "null", "value": Value::Null});
    }
    if s == "undefined" {
        return json!({"type": "undefined"});
    }
    if s == "true" {
        return json!({"type": "boolean", "value": true});
    }
    if s == "false" {
        return json!({"type": "boolean", "value": false});
    }
    if let Ok(n) = s.parse::<f64>() {
        // Reject Infinity / NaN (parse accepts them; CDP would JSON-encode).
        if n.is_finite() {
            return json!({"type": "number", "value": n});
        }
    }
    json!({"type": "string", "value": s})
}

async fn page_navigate(session: &mut Session, params: &Value) -> Result<Value, String> {
    let url = params
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| "Page.navigate: missing url".to_string())?;

    // Fetch the page, parse + load into the Runtime, run inline scripts.
    let fetcher = session.runtime.fetcher_clone();
    let mut req = bouncy_fetch::FetchRequest::new(url);
    for (k, v) in &session.extra_headers {
        req = req.header(k.clone(), v.clone());
    }
    let resp = fetcher
        .request(req)
        .await
        .map_err(|e| format!("fetch error: {e}"))?;

    let html =
        std::str::from_utf8(&resp.body).map_err(|e| format!("non-utf8 response body: {e}"))?;
    session
        .runtime
        .load(html, url)
        .map_err(|e| format!("load error: {e}"))?;
    if let Err(e) = session.runtime.run_inline_scripts() {
        return Ok(json!({
            "frameId": "frame-1",
            "loaderId": "loader-1",
            "errorText": format!("{e}"),
        }));
    }
    Ok(json!({
        "frameId": "frame-1",
        "loaderId": "loader-1",
    }))
}
