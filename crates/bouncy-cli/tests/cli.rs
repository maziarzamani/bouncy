//! Integration tests for the `bouncy` binary.
//!
//! Spins up a tiny hyper test server, runs the `bouncy` binary as a child
//! process, asserts on its stdout. Assumes `cargo test -p bouncy-cli` has
//! built the binary into target/<profile>/boink.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::process::{Command, Stdio};

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::Request;
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::TcpListener;

const STATIC_PAGE: &str = r#"<!doctype html>
<html><head><title>Demo</title></head>
<body>
  <h1>hello</h1>
  <a href="/about">About</a>
  <a href="https://example.com/help">Help</a>
</body></html>
"#;

async fn spawn_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let svc = service_fn(|_req: Request<Incoming>| async move {
                    Ok::<_, Infallible>(
                        hyper::Response::builder()
                            .status(200)
                            .header("content-type", "text/html")
                            .body(Full::new(Bytes::from_static(STATIC_PAGE.as_bytes())))
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

fn bouncy_bin() -> std::path::PathBuf {
    // CARGO_BIN_EXE_<binname> is a *compile-time* env var that Cargo
    // populates when building integration tests for a package with a
    // matching `[[bin]]`. Reading via `env!` resolves it at compile
    // time; `std::env::var` returns NotPresent at runtime.
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_bouncy"))
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bouncy_bin())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn bouncy")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_dump_html_emits_body() {
    let addr = spawn_server().await;
    let url = format!("http://{}/", addr);
    let out = run(&["fetch", &url, "--dump", "html", "--quiet"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = String::from_utf8_lossy(&out.stdout);
    assert!(body.contains("<h1>hello</h1>"), "body: {body}");
    assert!(body.contains("About"), "body: {body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_dump_links_resolves_against_url() {
    let addr = spawn_server().await;
    let url = format!("http://{}/", addr);
    let out = run(&["fetch", &url, "--dump", "links", "--quiet"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "stdout: {stdout}");
    assert!(
        lines[0].starts_with(&format!("http://{}/about", addr)),
        "got: {}",
        lines[0]
    );
    assert!(lines[0].ends_with("\tAbout"), "got: {}", lines[0]);
    assert!(
        lines[1].starts_with("https://example.com/help"),
        "got: {}",
        lines[1]
    );
    assert!(lines[1].ends_with("\tHelp"), "got: {}", lines[1]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_dump_text_strips_scripts() {
    let addr = spawn_server().await;
    let url = format!("http://{}/", addr);
    let out = run(&["fetch", &url, "--dump", "text", "--quiet"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = String::from_utf8_lossy(&out.stdout);
    assert!(body.contains("hello"));
    // <title>Demo</title> is in <head>, should be stripped from --dump text.
    assert!(
        !body.contains("Demo"),
        "title leaked into text output: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_stealth_hides_webdriver() {
    let addr = spawn_server().await;
    let url = format!("http://{}/", addr);
    let out = run(&[
        "fetch",
        &url,
        "--stealth",
        "--eval",
        "typeof navigator.webdriver",
        "--quiet",
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = String::from_utf8_lossy(&out.stdout);
    assert_eq!(body.trim(), "undefined");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_eval_runs_js_against_loaded_dom() {
    // One binary now: --eval boots V8 inline. (Previously this test
    // asserted that --eval errored with a "use bouncy-full" message;
    // bouncy-cli + bouncy-full are merged.)
    let addr = spawn_server().await;
    let url = format!("http://{}/", addr);
    let out = run(&["fetch", &url, "--eval", "document.title"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = String::from_utf8_lossy(&out.stdout);
    assert!(body.trim() == "Demo", "got: {body:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_output_to_file_writes_body_and_keeps_stdout_silent() {
    let addr = spawn_server().await;
    let url = format!("http://{}/", addr);
    let dir = std::env::temp_dir().join(format!(
        "bouncy-out-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let out_path = dir.join("body.html");
    let out = run(&["fetch", &url, "-o", out_path.to_str().unwrap(), "--quiet"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty(), "stdout should be silent with -o");
    let body = std::fs::read_to_string(&out_path).unwrap();
    assert!(body.contains("<h1>hello</h1>"), "got: {body}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_json_flag_sets_body_and_content_type() {
    let addr = spawn_echo_server().await;
    let url = format!("http://{}/api", addr);
    let out = run(&[
        "fetch",
        &url,
        "-X",
        "POST",
        "--json",
        r#"{"k":"v"}"#,
        "--dump",
        "html",
        "--quiet",
    ]);
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).expect("valid json echo");
    assert_eq!(v["method"], "POST");
    assert_eq!(v["body"], r#"{"k":"v"}"#);
    assert_eq!(v["headers"]["content-type"], "application/json");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_auth_flag_emits_basic_authorization_header() {
    let addr = spawn_echo_server().await;
    let url = format!("http://{}/", addr);
    let out = run(&[
        "fetch",
        &url,
        "--auth",
        "alice:s3cret",
        "--dump",
        "html",
        "--quiet",
    ]);
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).expect("valid json");
    // base64('alice:s3cret') = YWxpY2U6czNjcmV0
    assert_eq!(
        v["headers"]["authorization"], "Basic YWxpY2U6czNjcmV0",
        "got: {v}"
    );
}

async fn spawn_echo_server() -> SocketAddr {
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
                    use http_body_util::BodyExt;
                    let method = req.method().to_string();
                    let mut hs = serde_json::Map::new();
                    for (k, v) in req.headers().iter() {
                        hs.insert(
                            k.to_string(),
                            Value::String(v.to_str().unwrap_or("").into()),
                        );
                    }
                    let body_bytes = req.into_body().collect().await.unwrap().to_bytes();
                    let payload = serde_json::json!({
                        "method": method,
                        "headers": hs,
                        "body": String::from_utf8_lossy(&body_bytes),
                    })
                    .to_string();
                    Ok::<_, Infallible>(
                        hyper::Response::builder()
                            .status(200)
                            .header("content-type", "application/json")
                            .body(Full::new(Bytes::from(payload)))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_post_with_body_and_headers() {
    let addr = spawn_echo_server().await;
    let url = format!("http://{}/echo", addr);
    let out = run(&[
        "fetch",
        &url,
        "--method",
        "POST",
        "--body",
        r#"{"k":"v"}"#,
        "--header",
        "X-Bouncy-Test: 1",
        "--header",
        "Authorization: Bearer abc",
        "--dump",
        "html",
        "--quiet",
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).expect("valid json echo");
    assert_eq!(v["method"], "POST");
    assert_eq!(v["body"], r#"{"k":"v"}"#);
    assert_eq!(v["headers"]["x-bouncy-test"], "1");
    assert_eq!(v["headers"]["authorization"], "Bearer abc");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_get_with_custom_headers() {
    let addr = spawn_echo_server().await;
    let url = format!("http://{}/g", addr);
    let out = run(&[
        "fetch",
        &url,
        "--header",
        "X-Foo: bar",
        "--dump",
        "html",
        "--quiet",
    ]);
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).expect("valid json");
    assert_eq!(v["method"], "GET");
    assert_eq!(v["headers"]["x-foo"], "bar");
    assert_eq!(v["body"], "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scrape_text_format_per_url_line() {
    let addr = spawn_server().await;
    let urls = (0..3)
        .map(|i| format!("http://{}/p{}", addr, i))
        .collect::<Vec<_>>();
    let mut args = vec!["scrape"];
    args.extend(urls.iter().map(|s| s.as_str()));
    args.extend_from_slice(&["--concurrency", "3", "--format", "text"]);
    let out = run(&args);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "stdout: {stdout}");
    for line in &lines {
        // Format: "<N>ms\t<url>\t<title>"
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        assert_eq!(parts.len(), 3, "line: {line}");
        assert!(parts[0].ends_with("ms"), "parts: {parts:?}");
        assert!(parts[1].starts_with("http://"), "parts: {parts:?}");
        assert_eq!(parts[2], "Demo", "title parts: {parts:?}");
    }
}

async fn spawn_two_step_nav_server() -> SocketAddr {
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
                    let body: Bytes = match req.uri().path() {
                        "/a" => Bytes::from_static(
                            b"<html><head><title>A</title></head><body><script>location.href='/b';</script></body></html>",
                        ),
                        "/b" => Bytes::from_static(
                            b"<html><head><title>B</title></head><body>final</body></html>",
                        ),
                        _ => Bytes::from_static(b""),
                    };
                    Ok::<_, Infallible>(
                        hyper::Response::builder()
                            .status(200)
                            .header("content-type", "text/html")
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn location_href_set_follows_navigation() {
    let addr = spawn_two_step_nav_server().await;
    let url = format!("http://{}/a", addr);
    // /a's inline script sets `location.href = '/b'`; with --eval we
    // boot V8, run scripts, drain the queued nav, fetch /b, re-load,
    // and the eval result should reflect /b's title.
    let out = run(&["fetch", &url, "--eval", "document.title", "--quiet"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = String::from_utf8_lossy(&out.stdout);
    assert_eq!(body.trim(), "B", "got: {body:?}");
}

async fn spawn_flaky_server(fails_before_success: usize) -> SocketAddr {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let c = counter.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |_req: Request<Incoming>| {
                    let c2 = c.clone();
                    async move {
                        let n = c2.fetch_add(1, Ordering::SeqCst);
                        let (status, body): (u16, Bytes) = if n < fails_before_success {
                            (503, Bytes::from_static(b"<html><title>nope</title></html>"))
                        } else {
                            (
                                200,
                                Bytes::from_static(b"<html><title>RetryWin</title></html>"),
                            )
                        };
                        Ok::<_, Infallible>(
                            hyper::Response::builder()
                                .status(status)
                                .header("content-type", "text/html")
                                .body(Full::new(body))
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
async fn scrape_retry_recovers_from_transient_5xx() {
    let addr = spawn_flaky_server(2).await;
    let url = format!("http://{}/x", addr);
    let out = run(&[
        "scrape",
        &url,
        "--retry",
        "3",
        "--retry-delay-ms",
        "10",
        "--format",
        "json",
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).expect("valid json");
    let row = &v["results"].as_array().unwrap()[0];
    assert_eq!(row["title"], "RetryWin", "got: {row}");
    assert!(
        row["retries"].as_u64().unwrap() >= 2,
        "expected at least 2 retries, got: {row}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scrape_without_retry_records_zero_retries_on_5xx() {
    let addr = spawn_flaky_server(5).await;
    let url = format!("http://{}/x", addr);
    let out = run(&["scrape", &url, "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).expect("valid json");
    let row = &v["results"].as_array().unwrap()[0];
    assert_eq!(row["retries"].as_u64(), Some(0));
}

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
                            Some("token=abc; Path=/"),
                            Bytes::from_static(b"<html><title>in</title></html>"),
                        ),
                        "/me" => {
                            if cookie.contains("token=abc") {
                                (
                                    200,
                                    None,
                                    Bytes::from_static(b"<html><title>ok</title></html>"),
                                )
                            } else {
                                (
                                    401,
                                    None,
                                    Bytes::from_static(b"<html><title>no</title></html>"),
                                )
                            }
                        }
                        _ => (
                            404,
                            None,
                            Bytes::from_static(b"<html><title></title></html>"),
                        ),
                    };
                    let mut r = hyper::Response::builder().status(status);
                    if let Some(sc) = set_cookie {
                        r = r.header("Set-Cookie", sc);
                    }
                    Ok::<_, Infallible>(
                        r.header("content-type", "text/html")
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

async fn spawn_counted_server() -> (SocketAddr, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let h2 = h.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |_req: Request<Incoming>| {
                    let h3 = h2.clone();
                    async move {
                        h3.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, Infallible>(
                            hyper::Response::builder()
                                .status(200)
                                .header("content-type", "text/plain")
                                .body(Full::new(Bytes::from_static(b"hit")))
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
    (addr, hits)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_host_short_circuits_request() {
    use std::sync::atomic::Ordering;
    let (addr, hits) = spawn_counted_server().await;
    let url = format!("http://{}/track", addr);
    // --block-host accepts the literal `host:port` of the loopback server.
    let out = run(&[
        "fetch",
        &url,
        "--block-host",
        &format!("{addr}"),
        "--dump",
        "html",
        "--quiet",
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = String::from_utf8_lossy(&out.stdout);
    assert!(
        body.is_empty(),
        "expected blocked body to be empty, got: {body}"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "blocked URL should never reach the server"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cookie_jar_persists_across_cli_invocations() {
    // Two separate `bouncy fetch` processes share state through a JSON
    // jar file: process #1 hits /login (server sets a cookie), process
    // #2 hits /me which returns 401 unless the cookie replays.
    let addr = spawn_login_server().await;
    let dir = std::env::temp_dir().join(format!(
        "bouncy-jar-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let jar_path = dir.join("jar.json");

    // First invocation: log in, populate the jar.
    let login_url = format!("http://{}/login", addr);
    let out1 = run(&[
        "fetch",
        &login_url,
        "--cookie-jar",
        jar_path.to_str().unwrap(),
        "--quiet",
    ]);
    assert!(
        out1.status.success(),
        "login failed: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    assert!(jar_path.exists(), "jar file was not written");
    let saved = std::fs::read_to_string(&jar_path).unwrap();
    assert!(
        saved.contains("token") && saved.contains("abc"),
        "jar file missing cookie: {saved}"
    );

    // Second invocation: hit /me with the same jar. Should send the cookie
    // and get 200; without the jar it would be 401.
    let me_url = format!("http://{}/me", addr);
    let out2 = run(&[
        "fetch",
        &me_url,
        "--cookie-jar",
        jar_path.to_str().unwrap(),
        "--dump",
        "html",
        "--quiet",
    ]);
    assert!(
        out2.status.success(),
        "/me failed: stderr={}",
        String::from_utf8_lossy(&out2.stderr)
    );
    let body = String::from_utf8_lossy(&out2.stdout);
    assert!(
        body.contains("<title>ok</title>"),
        "/me did not see the cookie on the 2nd invocation: {body}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scrape_json_format_has_summary_and_results() {
    let addr = spawn_server().await;
    let urls = (0..2)
        .map(|i| format!("http://{}/p{}", addr, i))
        .collect::<Vec<_>>();
    let mut args = vec!["scrape"];
    args.extend(urls.iter().map(|s| s.as_str()));
    args.extend_from_slice(&["--concurrency", "2", "--format", "json"]);
    let out = run(&args);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).expect("valid json");
    assert_eq!(v["total_urls"], 2);
    assert_eq!(v["concurrency"], 2);
    assert!(v["total_time_ms"].is_number());
    assert!(v["avg_time_ms"].is_number());
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    for r in results {
        assert_eq!(r["title"], "Demo");
        assert!(r["eval"].is_null());
        assert!(r["time_ms"].is_number());
        assert!(r["worker"].is_number());
    }
}
