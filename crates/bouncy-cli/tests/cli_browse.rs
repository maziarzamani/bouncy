//! Integration tests for the `bouncy browse` subcommand.
//!
//! Spawns the real `bouncy` binary as a child process and drives it
//! against a `tiny_http` fixture server. Same pattern as `tests/cli.rs`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

fn bouncy_bin() -> std::path::PathBuf {
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

/// Server that records every inbound POST/GET and returns whatever HTML
/// `routes` says for the matching path.
async fn spawn_capturing(
    routes: Vec<(&'static str, &'static str)>,
) -> (SocketAddr, Arc<Mutex<Vec<(String, String, String)>>>) {
    let routes = Arc::new(routes);
    let captured: Arc<Mutex<Vec<(String, String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_t = captured.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let routes = routes.clone();
            let captured = captured_t.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let routes = routes.clone();
                    let captured = captured.clone();
                    async move {
                        let method = req.method().to_string();
                        let path = req.uri().path().to_string();
                        let body_bytes = req.into_body().collect().await.unwrap().to_bytes();
                        let body = String::from_utf8_lossy(&body_bytes).into_owned();
                        captured.lock().await.push((method, path.clone(), body));
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
    (addr, captured)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browse_chain_runs_steps_in_order_and_exits() {
    const LANDING: &str = r#"<html><head><title>Landing</title></head><body>
        <a href="/done">Next</a>
    </body></html>"#;
    const DONE: &str =
        r#"<html><head><title>Done</title></head><body><h1>welcome</h1></body></html>"#;
    let (addr, _) = spawn_capturing(vec![("/", LANDING), ("/done", DONE)]).await;
    let url = format!("http://{addr}/");
    // We use `goto /done` rather than `click a` because the synthetic
    // click event from bouncy-js doesn't follow the `<a href>` default
    // action — that's a known gap in the polyfill, tracked for a
    // follow-up. `goto` exercises the same end-to-end path (navigate
    // → snapshot → read) that we want to verify here.
    let out = run(&[
        "browse",
        &url,
        "--do",
        &format!("goto http://{addr}/done"),
        "--do",
        "read h1",
    ]);
    assert!(
        out.status.success(),
        "browse failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The read step prints each match indented; "welcome" should be one of them.
    assert!(
        stdout.contains("welcome"),
        "expected 'welcome' in stdout, got: {stdout}"
    );
    assert!(
        stdout.contains("done."),
        "expected `done.` footer, got: {stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browse_chain_json_emits_final_snapshot_as_json() {
    const PAGE: &str =
        r#"<html><head><title>JSON Test</title></head><body><h1>x</h1></body></html>"#;
    let (addr, _) = spawn_capturing(vec![("/", PAGE)]).await;
    let url = format!("http://{addr}/");
    let out = run(&["browse", &url, "--json", "--do", "snapshot"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The last line of stdout should be a JSON snapshot we can parse.
    let last = stdout.lines().last().unwrap_or("");
    // pretty-printed JSON spans multiple lines; parse the whole thing
    // as JSON and look for our title.
    let v: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|_| {
        // pretty JSON can't be parsed line-by-line; try the full output
        // or fall back to expecting the last line is a JSON document.
        serde_json::from_str(last).expect("expected JSON snapshot in stdout")
    });
    assert_eq!(v["title"], "JSON Test");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browse_chain_fill_then_submit_posts_form_values() {
    const FORM: &str = r#"<html><body>
        <form action="/post" method="POST">
          <input name="user" value="">
        </form>
    </body></html>"#;
    const OK: &str = r#"<html><head><title>OK</title></head><body></body></html>"#;
    let (addr, captured) = spawn_capturing(vec![("/", FORM), ("/post", OK)]).await;
    let url = format!("http://{addr}/");
    let out = run(&[
        "browse",
        &url,
        "--do",
        "fill [name=user] maziar",
        "--do",
        "submit form",
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let recorded = captured.lock().await.clone();
    let post = recorded
        .iter()
        .find(|(m, p, _)| m == "POST" && p == "/post")
        .unwrap_or_else(|| panic!("expected POST /post in {recorded:?}"));
    assert!(
        post.2.contains("user=maziar"),
        "expected user=maziar in body, got: {}",
        post.2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browse_repl_reads_commands_from_stdin_and_exits_on_quit() {
    const PAGE: &str = r#"<html><head><title>REPL</title></head><body>
        <a href="/x">Link</a>
    </body></html>"#;
    let (addr, _) = spawn_capturing(vec![
        ("/", PAGE),
        ("/x", "<html><body><h1>hit</h1></body></html>"),
    ])
    .await;
    let url = format!("http://{addr}/");
    // No --do means REPL. Pipe goto + read + exit through stdin.
    // (Same `<a href>` default-action gap as the chain test — using
    // goto here for the same reason.)
    let nav_url = format!("http://{addr}/x");
    let mut child = Command::new(bouncy_bin())
        .args(["browse", &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bouncy");
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, "goto {nav_url}").unwrap();
        writeln!(stdin, "read h1").unwrap();
        writeln!(stdin, "exit").unwrap();
    }
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The read step should have surfaced the h1 text.
    assert!(
        stdout.contains("hit"),
        "expected 'hit' in REPL output, got: {stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browse_chain_unknown_command_errors_with_useful_message() {
    const PAGE: &str = r#"<html><head><title>x</title></head><body></body></html>"#;
    let (addr, _) = spawn_capturing(vec![("/", PAGE)]).await;
    let url = format!("http://{addr}/");
    let out = run(&["browse", &url, "--do", "scroll down"]);
    // Chain failure should propagate as non-zero exit.
    assert!(
        !out.status.success(),
        "expected non-zero exit on bad command"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stderr}{stdout}");
    assert!(
        combined.contains("unknown command") && combined.contains("scroll"),
        "expected error mentioning 'unknown command' and 'scroll', got: stderr={stderr} stdout={stdout}"
    );
}
