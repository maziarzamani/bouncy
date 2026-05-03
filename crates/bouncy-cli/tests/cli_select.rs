//! Integration tests for the new flags landed in PR A:
//! `--select` / `--attr`, `--per-host-concurrency`, `--user-agent`.
//!
//! Spins up a hyper test server and runs the real `bouncy` binary as a
//! child process. Same pattern as `tests/cli.rs`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::Request;
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::TcpListener;

const SAMPLE_PAGE: &str = r#"<!doctype html>
<html><head><title>Demo</title></head>
<body>
  <h1 class="headline">Hello world</h1>
  <h1 class="sub">Subtitle</h1>
  <a class="link" href="https://a.example">Link A</a>
  <a class="link" href="https://b.example">Link B</a>
  <span data-price="9.99">Cheap</span>
</body></html>
"#;

fn bouncy_bin() -> std::path::PathBuf {
    // Compile-time resolution — see the longer comment in tests/cli.rs.
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

async fn spawn_static_server(body: &'static str) -> SocketAddr {
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
                        hyper::Response::builder()
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

/// Server that records the User-Agent of each request and echoes it
/// back in the body — lets us assert what the binary actually sent.
async fn spawn_ua_echo() -> SocketAddr {
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
                    let ua = req
                        .headers()
                        .get(hyper::header::USER_AGENT)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<missing>")
                        .to_string();
                    Ok::<_, Infallible>(
                        hyper::Response::builder()
                            .status(200)
                            .header("content-type", "text/plain")
                            .body(Full::new(Bytes::from(ua)))
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
async fn fetch_select_emits_text_per_match_one_per_line() {
    let addr = spawn_static_server(SAMPLE_PAGE).await;
    let url = format!("http://{}/", addr);
    let out = run(&["fetch", &url, "--select", "h1"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines, vec!["Hello world", "Subtitle"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_select_with_attr_emits_attribute_values() {
    // bouncy-dom's selector grammar today is single-clause (tag OR
    // `.class` OR `#id`), so we use the bare `a` here. Compound
    // selectors like `a.link` need extending the grammar — separate PR.
    let addr = spawn_static_server(SAMPLE_PAGE).await;
    let url = format!("http://{}/", addr);
    let out = run(&["fetch", &url, "--select", "a", "--attr", "href"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines, vec!["https://a.example", "https://b.example"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_select_no_match_succeeds_with_empty_output() {
    let addr = spawn_static_server(SAMPLE_PAGE).await;
    let url = format!("http://{}/", addr);
    let out = run(&["fetch", &url, "--select", "nonexistent"]);
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "expected empty stdout, got {:?}",
        out.stdout
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scrape_select_populates_selected_field_in_json() {
    let addr = spawn_static_server(SAMPLE_PAGE).await;
    let url = format!("http://{}/", addr);
    // `.headline` is a single-clause class selector — supported today.
    let out = run(&["scrape", &url, "--select", ".headline"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).expect("scrape output is JSON");
    let row = &v["results"][0];
    assert_eq!(row["selected"][0], "Hello world");
    assert_eq!(row["selected"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scrape_without_select_omits_selected_field() {
    // When --select isn't passed, the JSON shouldn't carry a `selected: null`
    // — `#[serde(skip_serializing_if = "Option::is_none")]` keeps it absent.
    let addr = spawn_static_server(SAMPLE_PAGE).await;
    let url = format!("http://{}/", addr);
    let out = run(&["scrape", &url]);
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let row = &v["results"][0];
    assert!(
        row.get("selected").is_none(),
        "expected `selected` field to be absent, got: {row}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_user_agent_default_identifies_as_bouncy() {
    let addr = spawn_ua_echo().await;
    let url = format!("http://{}/", addr);
    let out = run(&["fetch", &url, "--quiet"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = String::from_utf8_lossy(&out.stdout);
    assert!(
        body.starts_with("bouncy/"),
        "expected default UA to start with `bouncy/`, got: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_user_agent_flag_overrides_default() {
    let addr = spawn_ua_echo().await;
    let url = format!("http://{}/", addr);
    let out = run(&["fetch", &url, "--user-agent", "TestBot/9.9", "--quiet"]);
    assert!(out.status.success());
    let body = String::from_utf8_lossy(&out.stdout);
    assert_eq!(body.trim_end(), "TestBot/9.9");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scrape_per_host_concurrency_limits_inflight_against_one_host() {
    // Server tracks the maximum concurrent in-flight count it ever saw.
    // With --concurrency 5 --per-host-concurrency 2 against 5 URLs on one
    // host, the high-water mark must be ≤ 2.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let in_flight_t = in_flight.clone();
    let max_seen_t = max_seen.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let in_flight = in_flight_t.clone();
            let max_seen = max_seen_t.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |_req: Request<Incoming>| {
                    let in_flight = in_flight.clone();
                    let max_seen = max_seen.clone();
                    async move {
                        let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                        // Update high-water mark.
                        let mut prev = max_seen.load(Ordering::SeqCst);
                        while now > prev {
                            match max_seen.compare_exchange(
                                prev,
                                now,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            ) {
                                Ok(_) => break,
                                Err(p) => prev = p,
                            }
                        }
                        // Hold the request open long enough that other
                        // in-flight requests overlap if not throttled.
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                        Ok::<_, Infallible>(
                            hyper::Response::builder()
                                .status(200)
                                .body(Full::new(Bytes::from_static(
                                    b"<html><title>x</title></html>",
                                )))
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

    let urls: Vec<String> = (0..5).map(|i| format!("http://{}/p{}", addr, i)).collect();
    let mut args: Vec<&str> = vec![
        "scrape",
        "--concurrency",
        "5",
        "--per-host-concurrency",
        "2",
    ];
    for u in &urls {
        args.push(u);
    }
    let out = run(&args);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let max = max_seen.load(Ordering::SeqCst);
    assert!(
        max <= 2,
        "per-host throttle didn't hold; max in-flight = {max}, expected ≤ 2"
    );
    assert!(max >= 1, "no requests reached the server (max={max})");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scrape_without_per_host_throttle_allows_higher_concurrency() {
    // Sanity check the inverse: without --per-host-concurrency, the same
    // 5 URLs against one host should saturate up to --concurrency.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let in_flight_t = in_flight.clone();
    let max_seen_t = max_seen.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let in_flight = in_flight_t.clone();
            let max_seen = max_seen_t.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |_req: Request<Incoming>| {
                    let in_flight = in_flight.clone();
                    let max_seen = max_seen.clone();
                    async move {
                        let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                        let mut prev = max_seen.load(Ordering::SeqCst);
                        while now > prev {
                            match max_seen.compare_exchange(
                                prev,
                                now,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            ) {
                                Ok(_) => break,
                                Err(p) => prev = p,
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                        Ok::<_, Infallible>(
                            hyper::Response::builder()
                                .status(200)
                                .body(Full::new(Bytes::from_static(
                                    b"<html><title>x</title></html>",
                                )))
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
    let urls: Vec<String> = (0..5).map(|i| format!("http://{}/p{}", addr, i)).collect();
    let mut args: Vec<&str> = vec!["scrape", "--concurrency", "5"];
    for u in &urls {
        args.push(u);
    }
    let out = run(&args);
    assert!(out.status.success());
    let max = max_seen.load(Ordering::SeqCst);
    // Without the throttle we expect to have seen MORE than 2 in-flight.
    // We don't assert exactly 5 because the binary's startup latency
    // can de-sync the launches; ≥3 is enough to prove the throttle
    // was responsible for the difference in the previous test.
    assert!(
        max >= 3,
        "without throttle expected high-water ≥ 3, got {max} (something else might be limiting concurrency)"
    );
}
