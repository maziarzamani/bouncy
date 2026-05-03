//! Integration tests for `BrowseSession::submit`. Covers the three
//! branches in its docstring:
//!   1. Form has `action` attr → real HTTP request (POST or GET) built
//!      from form fields, response loaded into the session.
//!   2. Form has no `action` (JS-only handler) → `submit` event
//!      dispatched; any `location.href` redirects drained.
//!   3. Selector targets a submit `<button>` not a `<form>` → climb to
//!      the enclosing form, then apply branches 1 or 2.
//!
//! Plus error cases: selector matches nothing, selector matches an
//! element with no enclosing form.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bouncy_browse::{BrowseOpts, BrowseSession, ReadMode};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// Captured request snapshot for assertions.
#[derive(Default, Clone)]
struct Captured {
    method: String,
    path: String,
    query: String,
    content_type: String,
    body: String,
}

/// Spawn a server that records every inbound request (method, path,
/// query string, content-type, body) and returns whatever HTML the
/// `responder` closure emits for that path.
async fn spawn_capturing<F>(
    routes: Vec<(&'static str, &'static str)>,
    captured: Arc<Mutex<Vec<Captured>>>,
    _phantom: F,
) -> SocketAddr
where
    F: Send + Sync + 'static,
{
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
            let captured = captured.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let routes = routes.clone();
                    let captured = captured.clone();
                    async move {
                        let method = req.method().to_string();
                        let path = req.uri().path().to_string();
                        let query = req.uri().query().unwrap_or("").to_string();
                        let content_type = req
                            .headers()
                            .get(hyper::header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        let body_bytes = req.into_body().collect().await.unwrap().to_bytes();
                        let body = String::from_utf8_lossy(&body_bytes).into_owned();
                        captured.lock().await.push(Captured {
                            method,
                            path: path.clone(),
                            query,
                            content_type,
                            body,
                        });
                        let html = routes
                            .iter()
                            .find(|(p, _)| *p == path)
                            .map(|(_, b)| *b)
                            .unwrap_or("<html><body>404</body></html>");
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .header("content-type", "text/html")
                                .body(Full::new(Bytes::from_static(html.as_bytes())))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_post_form_sends_urlencoded_body_with_form_fields() {
    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let landing = r#"<!doctype html>
<html><body>
  <form id="signup" action="/welcome" method="POST">
    <input name="user" value="">
    <input name="email" value="">
  </form>
</body></html>"#;
    let welcome = r#"<!doctype html><html><head><title>OK</title></head><body><h1>welcome</h1></body></html>"#;
    let addr = spawn_capturing(
        vec![("/", landing), ("/welcome", welcome)],
        captured.clone(),
        (),
    )
    .await;

    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    session.fill("[name=user]", "maziar").await.unwrap();
    session.fill("[name=email]", "x@y.test").await.unwrap();
    let snap = session.submit("#signup").await.unwrap();
    assert_eq!(snap.title, "OK");
    let recorded = captured.lock().await.clone();
    // Two requests: GET / for the landing page, POST /welcome for the form.
    let post = recorded
        .iter()
        .find(|c| c.method == Method::POST.as_str() && c.path == "/welcome")
        .expect("expected a POST /welcome request, got: {recorded:?}");
    assert!(
        post.content_type
            .starts_with("application/x-www-form-urlencoded"),
        "wrong content-type: {}",
        post.content_type
    );
    assert!(
        post.body.contains("user=maziar"),
        "expected user=maziar in body, got: {}",
        post.body
    );
    assert!(
        post.body.contains("email=x%40y.test"),
        "expected url-encoded email in body, got: {}",
        post.body
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_get_form_appends_query_string_to_action_url() {
    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let landing = r#"<!doctype html>
<html><body>
  <form id="search" action="/results" method="GET">
    <input name="q" value="">
    <input name="lang" value="en">
  </form>
</body></html>"#;
    let results = r#"<html><head><title>Results</title></head><body><h1>res</h1></body></html>"#;
    let addr = spawn_capturing(
        vec![("/", landing), ("/results", results)],
        captured.clone(),
        (),
    )
    .await;

    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    session.fill("[name=q]", "rust").await.unwrap();
    let snap = session.submit("#search").await.unwrap();
    assert_eq!(snap.title, "Results");

    let recorded = captured.lock().await.clone();
    let get_results = recorded
        .iter()
        .find(|c| c.method == Method::GET.as_str() && c.path == "/results")
        .expect("expected GET /results");
    assert!(
        get_results.query.contains("q=rust"),
        "expected q=rust in query, got: {}",
        get_results.query
    );
    assert!(
        get_results.query.contains("lang=en"),
        "expected lang=en in query, got: {}",
        get_results.query
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_when_selector_is_button_climbs_to_enclosing_form() {
    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let landing = r#"<!doctype html>
<html><body>
  <form action="/done" method="POST">
    <input name="msg" value="">
    <button id="go" type="submit">Go</button>
  </form>
</body></html>"#;
    let done = r#"<html><head><title>Done</title></head><body></body></html>"#;
    let addr = spawn_capturing(vec![("/", landing), ("/done", done)], captured.clone(), ()).await;

    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    session.fill("[name=msg]", "hi").await.unwrap();
    // Selector is the BUTTON, not the form — the primitive should climb.
    let snap = session.submit("#go").await.unwrap();
    assert_eq!(snap.title, "Done");
    let recorded = captured.lock().await.clone();
    let post = recorded
        .iter()
        .find(|c| c.method == "POST" && c.path == "/done")
        .expect("expected POST /done");
    assert!(post.body.contains("msg=hi"), "got body: {}", post.body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_form_without_action_dispatches_submit_event() {
    // Form has no `action` attr — the page's JS handler captures the
    // submit and writes to a hidden input we can read back. No HTTP
    // request to a /welcome path; we stay on the same page.
    let landing = r#"<!doctype html>
<html><body>
  <form id="js-form">
    <input id="trace" name="trace" value="">
    <button type="submit">Send</button>
  </form>
  <script>
    document.querySelector('#js-form').addEventListener('submit', function(e) {
      e.preventDefault();
      document.querySelector('#trace').value = 'submitted';
    });
  </script>
</body></html>"#;
    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let addr = spawn_capturing(vec![("/", landing)], captured.clone(), ()).await;

    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    session.submit("#js-form").await.unwrap();
    // The handler set the trace input to "submitted" — read it back.
    let trace = session
        .read("[name=trace]", ReadMode::Attr("value".into()))
        .await
        .unwrap();
    assert_eq!(trace, vec!["submitted"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_skips_unchecked_checkboxes_and_disabled_fields() {
    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let landing = r#"<!doctype html>
<html><body>
  <form action="/x" method="POST">
    <input name="keep" value="yes">
    <input name="disabled_field" value="no" disabled>
    <input type="checkbox" name="opted_in">
    <input type="checkbox" name="newsletter" checked>
  </form>
</body></html>"#;
    let ok = r#"<html><head><title>OK</title></head><body></body></html>"#;
    let addr = spawn_capturing(vec![("/", landing), ("/x", ok)], captured.clone(), ()).await;

    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    session.submit("form").await.unwrap();
    let recorded = captured.lock().await.clone();
    let post = recorded
        .iter()
        .find(|c| c.method == "POST" && c.path == "/x")
        .expect("expected POST /x");
    assert!(post.body.contains("keep=yes"), "got: {}", post.body);
    assert!(
        !post.body.contains("disabled_field"),
        "disabled fields should be skipped, got: {}",
        post.body
    );
    assert!(
        !post.body.contains("opted_in"),
        "unchecked checkbox should be skipped, got: {}",
        post.body
    );
    assert!(
        post.body.contains("newsletter="),
        "checked checkbox should be included, got: {}",
        post.body
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_no_match_returns_typed_error() {
    let landing = r#"<html><body><form action="/x"></form></body></html>"#;
    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let addr = spawn_capturing(vec![("/", landing)], captured.clone(), ()).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    let err = session.submit("#nope").await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("matched no elements") || msg.contains("no match"),
        "expected NoMatch-shaped error, got: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_selector_outside_any_form_returns_clear_error() {
    let landing = r#"<html><body><div id="loose">not in a form</div></body></html>"#;
    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let addr = spawn_capturing(vec![("/", landing)], captured.clone(), ()).await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/"), BrowseOpts::default())
        .await
        .unwrap();
    let err = session.submit("#loose").await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not a form") || msg.contains("not inside"),
        "expected 'not in form' error, got: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_relative_action_resolves_against_current_url() {
    // Form's action is just "do" — should resolve against the current
    // page URL (path-relative), not against the action root.
    let landing = r#"<html><body>
        <form action="do" method="GET">
          <input name="k" value="v">
        </form>
    </body></html>"#;
    let response = r#"<html><head><title>OK</title></head><body></body></html>"#;
    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let addr = spawn_capturing(
        vec![("/start/", landing), ("/start/do", response)],
        captured.clone(),
        (),
    )
    .await;
    let (session, _) = BrowseSession::open(&format!("http://{addr}/start/"), BrowseOpts::default())
        .await
        .unwrap();
    let snap = session.submit("form").await.unwrap();
    assert_eq!(snap.title, "OK");
    let recorded = captured.lock().await.clone();
    assert!(
        recorded
            .iter()
            .any(|c| c.path == "/start/do" && c.query == "k=v"),
        "expected GET /start/do?k=v, got: {:?}",
        recorded
            .iter()
            .map(|c| format!("{} {} ?{}", c.method, c.path, c.query))
            .collect::<Vec<_>>()
    );
}
