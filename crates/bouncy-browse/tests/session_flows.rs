//! Integration tests for `BrowseSession`. Spins up a hyper server on a
//! random port and drives a session through realistic multi-step flows.
//!
//! These exist alongside the unit tests in `src/snapshot.rs` and `src/session.rs`
//! to cover the wiring between the V8 runtime, the HTTP client, the
//! navigation drain loop, and the snapshot generator.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bouncy_browse::{BrowseOpts, BrowseSession, ReadMode};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

/// Spawn a hyper server that returns a constant body for every request.
async fn spawn_static(body: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let svc = service_fn(move |_req: Request<Incoming>| async move {
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(200)
                            .header("content-type", "text/html")
                            .body(Full::new(Bytes::from_static(body.as_bytes())))
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

/// Spawn a multi-page server with a path-based router. Each entry in
/// `routes` maps a request path to a body to return.
async fn spawn_router(routes: Vec<(&'static str, &'static str)>) -> SocketAddr {
    let routes = Arc::new(routes);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let routes = routes.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let routes = routes.clone();
                    async move {
                        let path = req.uri().path().to_string();
                        let body: &'static str = routes
                            .iter()
                            .find(|(p, _)| *p == path)
                            .map(|(_, b)| *b)
                            .unwrap_or("<html><body>404</body></html>");
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .header("content-type", "text/html")
                                .body(Full::new(Bytes::from_static(body.as_bytes())))
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
    addr
}

/// Spawn a server that records every inbound request's `Cookie` header
/// for later inspection. Returns `(addr, captured_cookies)`.
#[allow(clippy::type_complexity)]
async fn spawn_cookie_echo() -> (SocketAddr, Arc<tokio::sync::Mutex<Vec<String>>>) {
    let captured: Arc<tokio::sync::Mutex<Vec<String>>> = Arc::new(Default::default());
    let captured_t = captured.clone();
    let hits = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let captured = captured_t.clone();
            let hits = hits.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let captured = captured.clone();
                    let hits = hits.clone();
                    async move {
                        let cookie_header = req
                            .headers()
                            .get(hyper::header::COOKIE)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        captured.lock().await.push(cookie_header);
                        let n = hits.fetch_add(1, Ordering::SeqCst);
                        // First hit: set a cookie and return a tiny page
                        // with a link to /next. Subsequent hits: just a page.
                        let resp = if n == 0 {
                            Response::builder()
                                .status(200)
                                .header("content-type", "text/html")
                                .header("set-cookie", "session=abc123; Path=/")
                                .body(Full::new(Bytes::from_static(
                                    b"<html><body><h1>landed</h1></body></html>",
                                )))
                                .unwrap()
                        } else {
                            Response::builder()
                                .status(200)
                                .header("content-type", "text/html")
                                .body(Full::new(Bytes::from_static(
                                    b"<html><body><h1>second</h1></body></html>",
                                )))
                                .unwrap()
                        };
                        let _ = req.into_body().collect().await;
                        Ok::<_, Infallible>(resp)
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    (addr, captured)
}

const LANDING_PAGE: &str = r#"<!doctype html>
<html><head><title>Landing</title></head>
<body>
  <h1>Welcome</h1>
  <a href="/signup">Sign up</a>
  <a href="/about">About</a>
</body></html>"#;

const SIGNUP_PAGE: &str = r#"<!doctype html>
<html><head><title>Sign up</title></head>
<body>
  <h1>Create an account</h1>
  <form id="signup" action="/welcome" method="GET">
    <label for="u">Username</label>
    <input id="u" name="user" type="text">
    <input name="email" type="email" placeholder="you@example.com">
    <button type="submit">Submit</button>
  </form>
</body></html>"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_returns_initial_snapshot_with_title_and_links() {
    let addr = spawn_static(LANDING_PAGE).await;
    let url = format!("http://{addr}/");
    let (_session, snap) = BrowseSession::open(&url, BrowseOpts::default())
        .await
        .expect("open");
    assert_eq!(snap.title, "Landing");
    assert_eq!(snap.url, url);
    assert!(
        snap.links.iter().any(|l| l.text == "Sign up"),
        "expected Sign up link, got: {:?}",
        snap.links
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goto_navigates_in_place_and_returns_new_snapshot() {
    let addr = spawn_router(vec![("/", LANDING_PAGE), ("/signup", SIGNUP_PAGE)]).await;
    let (session, snap) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    assert_eq!(snap.title, "Landing");
    let snap2 = session
        .goto(&format!("http://{addr}/signup"))
        .await
        .unwrap();
    assert_eq!(snap2.title, "Sign up");
    assert_eq!(snap2.forms.len(), 1);
    assert_eq!(snap2.forms[0].selector, "#signup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fill_writes_value_visible_to_subsequent_read() {
    let addr = spawn_static(SIGNUP_PAGE).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    session.fill("#u", "maziar").await.unwrap();
    // After fill, the value attribute should reflect the change. We can
    // observe it by reading the input via JS (the V8 IDL setter on .value
    // also writes the attribute via bouncy-bridge).
    let result = session
        .eval("document.querySelector('#u').value")
        .await
        .unwrap();
    assert_eq!(result.result.trim_matches('"'), "maziar");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn click_fires_synthetic_event_observable_via_js_handler() {
    // Page registers a click handler that writes a value into a hidden
    // input. After click(), eval that input's value to verify the
    // handler fired.
    const PAGE: &str = r#"<!doctype html>
<html><body>
  <button id="b">go</button>
  <input id="out" name="out" value="">
  <script>
    document.querySelector('#b').addEventListener('click', function() {
      document.querySelector('#out').value = 'clicked';
    });
  </script>
</body></html>"#;
    let addr = spawn_static(PAGE).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    session.click("#b").await.unwrap();
    let res = session
        .eval("document.querySelector('#out').value")
        .await
        .unwrap();
    assert_eq!(res.result.trim_matches('"'), "clicked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fill_dispatches_input_and_change_events() {
    // Page increments a counter on input + change. After fill, the
    // counter should be 2 (one input event, one change event).
    const PAGE: &str = r#"<!doctype html>
<html><body>
  <input id="x" name="x">
  <input id="counter" name="counter" value="0">
  <script>
    var inp = document.querySelector('#x');
    var cnt = document.querySelector('#counter');
    function bump() { cnt.value = String((parseInt(cnt.value, 10) || 0) + 1); }
    inp.addEventListener('input', bump);
    inp.addEventListener('change', bump);
  </script>
</body></html>"#;
    let addr = spawn_static(PAGE).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    session.fill("#x", "hello").await.unwrap();
    let res = session
        .eval("document.querySelector('#counter').value")
        .await
        .unwrap();
    assert_eq!(res.result.trim_matches('"'), "2", "expected 2 events fired");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_returns_text_html_and_attribute_modes() {
    const PAGE: &str = r#"<!doctype html>
<html><body>
  <a href="/x">First</a>
  <a href="/y">Second</a>
</body></html>"#;
    let addr = spawn_static(PAGE).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    let texts = session.read("a", ReadMode::Text).await.unwrap();
    assert_eq!(texts, vec!["First", "Second"]);
    let hrefs = session
        .read("a", ReadMode::Attr("href".into()))
        .await
        .unwrap();
    assert_eq!(hrefs, vec!["/x", "/y"]);
    let htmls = session.read("a", ReadMode::Html).await.unwrap();
    assert_eq!(htmls.len(), 2);
    assert!(htmls[0].contains("href=\"/x\""), "got: {}", htmls[0]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn click_on_missing_selector_returns_typed_no_match_error() {
    let addr = spawn_static(LANDING_PAGE).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    let err = session.click("#does-not-exist").await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("matched no elements") || msg.contains("no match"),
        "expected NoMatch-shaped error, got: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cookie_set_on_first_hit_replays_on_subsequent_goto() {
    let (addr, captured) = spawn_cookie_echo().await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    // Second page in the same session — server stamped a Set-Cookie on
    // hit #0; bouncy-fetch should replay it on hit #1.
    session.goto(&format!("http://{addr}/next")).await.unwrap();
    let recorded = captured.lock().await.clone();
    assert_eq!(recorded.len(), 2, "expected exactly 2 inbound hits");
    assert!(recorded[0].is_empty(), "first hit should have no cookie");
    assert!(
        recorded[1].contains("session=abc123"),
        "second hit should carry the cookie set on hit #0, got: {:?}",
        recorded[1]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_returns_current_page_state_unchanged() {
    let addr = spawn_static(SIGNUP_PAGE).await;
    let (session, snap1) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    let snap2 = session.snapshot().await.unwrap();
    assert_eq!(snap1.title, snap2.title);
    assert_eq!(snap1.forms.len(), snap2.forms.len());
}

// =============================================================================
//  Tests for primitives adopted from browser-use:
//  click_text, indexed targets, select_option, press_key, wait_for, back/forward,
//  chain.
// =============================================================================

const SEARCH_PAGE: &str = r#"<!doctype html>
<html><head><title>Search</title></head>
<body>
  <h1>Search</h1>
  <a href="/results">Results</a>
  <a href="/help">Help</a>
  <button>Go</button>
  <button>Cancel</button>
</body></html>"#;

const DROPDOWN_PAGE: &str = r#"<!doctype html>
<html><head><title>Pick</title></head>
<body>
  <form action="/picked" method="GET">
    <select id="topic" name="topic">
      <option value="a">Apples</option>
      <option value="b">Bananas</option>
      <option value="c">Cherries</option>
    </select>
    <button type="submit">Submit</button>
  </form>
</body></html>"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn click_text_matches_button_by_visible_text() {
    let addr = spawn_static(SEARCH_PAGE).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    // "Go" is the first button — click_text should resolve to it.
    let snap = session.click_text("Go").await.unwrap();
    // We're on a static-fixture server so the page doesn't navigate; we
    // just confirm the call succeeded and didn't error on the selector.
    assert_eq!(snap.title, "Search");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn click_text_is_case_insensitive_and_trimmed() {
    let addr = spawn_static(SEARCH_PAGE).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    session.click_text("  cAnCeL ").await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn click_text_no_match_returns_typed_error() {
    use bouncy_browse::BrowseError;
    let addr = spawn_static(SEARCH_PAGE).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    let err = session.click_text("Dropbox").await.unwrap_err();
    assert!(matches!(err, BrowseError::NoMatch(_)), "got: {err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fill_target_by_index_resolves_against_current_snapshot() {
    use bouncy_browse::Target;
    let addr = spawn_static(SIGNUP_PAGE).await;
    let (session, snap) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    // Pick the email field by index — it has placeholder "you@example.com".
    let email_idx = snap.forms[0]
        .fields
        .iter()
        .find(|f| f.placeholder.as_deref() == Some("you@example.com"))
        .unwrap()
        .index;
    let snap2 = session
        .fill_target(Target::Index(email_idx), "me@x.test")
        .await
        .unwrap();
    // After the fill, dump_html re-emits the input with value=me@x.test.
    let same_idx_field = snap2.forms[0]
        .fields
        .iter()
        .find(|f| f.placeholder.as_deref() == Some("you@example.com"))
        .unwrap();
    assert_eq!(same_idx_field.value.as_deref(), Some("me@x.test"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fill_target_by_index_unknown_returns_no_match() {
    use bouncy_browse::{BrowseError, Target};
    let addr = spawn_static(SIGNUP_PAGE).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    let err = session
        .fill_target(Target::Index(9999), "x")
        .await
        .unwrap_err();
    assert!(matches!(err, BrowseError::NoMatch(_)), "got: {err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn select_option_by_value_sets_select_value() {
    use bouncy_browse::Target;
    let addr = spawn_static(DROPDOWN_PAGE).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    session
        .select_option(Target::selector("#topic"), "b")
        .await
        .unwrap();
    // Read the select's value back via eval.
    let r = session
        .eval("document.querySelector('#topic').value")
        .await
        .unwrap();
    assert!(r.result.contains('b'), "got: {}", r.result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn select_option_by_text_falls_back_when_value_doesnt_match() {
    use bouncy_browse::Target;
    let addr = spawn_static(DROPDOWN_PAGE).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    // "Cherries" matches the option text, not its value.
    session
        .select_option(Target::selector("#topic"), "Cherries")
        .await
        .unwrap();
    let r = session
        .eval("document.querySelector('#topic').value")
        .await
        .unwrap();
    assert!(r.result.contains('c'), "got: {}", r.result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn select_option_unknown_value_returns_no_match() {
    use bouncy_browse::{BrowseError, Target};
    let addr = spawn_static(DROPDOWN_PAGE).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    let err = session
        .select_option(Target::selector("#topic"), "durians")
        .await
        .unwrap_err();
    assert!(matches!(err, BrowseError::NoMatch(_)), "got: {err}");
}

const KEY_PAGE: &str = r#"<!doctype html>
<html><head><title>Keys</title></head>
<body>
  <input id="x" name="x">
  <div id="last">none</div>
  <script>
    var el = document.querySelector('#x');
    el.addEventListener('keydown', function(e) {
      document.querySelector('#last').textContent = 'down:' + e.key;
    });
  </script>
</body></html>"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn press_key_dispatches_keydown_visible_to_handler() {
    use bouncy_browse::Target;
    let addr = spawn_static(KEY_PAGE).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    session
        .press_key(Target::selector("#x"), "Enter")
        .await
        .unwrap();
    let reads = session
        .read("#last", bouncy_browse::ReadMode::Text)
        .await
        .unwrap();
    assert_eq!(reads, vec!["down:Enter".to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_for_returns_immediately_when_selector_already_matches() {
    let addr = spawn_static(SIGNUP_PAGE).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    let snap = session.wait_for("#signup", 1000).await.unwrap();
    assert_eq!(snap.forms.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_for_times_out_when_selector_never_matches() {
    use bouncy_browse::BrowseError;
    let addr = spawn_static(SIGNUP_PAGE).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    let err = session.wait_for(".never-here", 100).await.unwrap_err();
    assert!(matches!(err, BrowseError::NoMatch(_)), "got: {err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_for_text_finds_existing_text() {
    let addr = spawn_static(SIGNUP_PAGE).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    let snap = session
        .wait_for_text("create an account", 500)
        .await
        .unwrap();
    assert_eq!(snap.title, "Sign up");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn back_forward_traverse_session_history() {
    use bouncy_browse::BrowseError;
    let addr = spawn_router(vec![("/", LANDING_PAGE), ("/signup", SIGNUP_PAGE)]).await;
    let (session, snap) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    assert_eq!(snap.title, "Landing");

    // No history yet.
    assert!(matches!(
        session.back().await.unwrap_err(),
        BrowseError::Io(_)
    ));

    let s2 = session
        .goto(&format!("http://{addr}/signup"))
        .await
        .unwrap();
    assert_eq!(s2.title, "Sign up");

    let back = session.back().await.unwrap();
    assert_eq!(back.title, "Landing");

    let fwd = session.forward().await.unwrap();
    assert_eq!(fwd.title, "Sign up");

    // Forward stack drained.
    assert!(matches!(
        session.forward().await.unwrap_err(),
        BrowseError::Io(_)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chain_runs_multiple_steps_in_one_round_trip() {
    use bouncy_browse::{ChainStep, ChainStepOutput, Target};
    let addr = spawn_static(SIGNUP_PAGE).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    let outs = session
        .chain(vec![
            ChainStep::Fill {
                target: Target::selector("#u"),
                value: "alice".into(),
            },
            ChainStep::Fill {
                target: Target::selector("[name=email]"),
                value: "a@b.test".into(),
            },
            ChainStep::Snapshot,
        ])
        .await
        .unwrap();
    assert_eq!(outs.len(), 3);
    let last = match &outs[2] {
        ChainStepOutput::Snapshot(s) => s,
        other => panic!("expected snapshot, got: {other:?}"),
    };
    let user_field = last.forms[0]
        .fields
        .iter()
        .find(|f| f.name.as_deref() == Some("user"))
        .unwrap();
    assert_eq!(user_field.value.as_deref(), Some("alice"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chain_stops_at_first_error() {
    use bouncy_browse::{BrowseError, ChainStep, Target};
    let addr = spawn_static(SIGNUP_PAGE).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    let err = session
        .chain(vec![
            ChainStep::Fill {
                target: Target::selector("#u"),
                value: "alice".into(),
            },
            ChainStep::Click(Target::selector(".not-here")),
            ChainStep::Snapshot,
        ])
        .await
        .unwrap_err();
    assert!(matches!(err, BrowseError::NoMatch(_)), "got: {err}");
}
