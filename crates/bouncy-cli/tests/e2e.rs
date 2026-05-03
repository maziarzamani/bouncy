//! Real-network E2E smokes for PR A's new flags. These hit public
//! endpoints (example.com, httpbin.org), so they're gated behind the
//! `e2e` Cargo feature to keep `cargo test` runnable offline / in
//! sandboxed CI.
//!
//! Run with: `cargo test --features e2e --test e2e`.

#![cfg(feature = "e2e")]

use std::process::{Command, Stdio};

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

#[test]
fn select_h1_against_example_com_returns_example_domain() {
    let out = run(&["fetch", "https://example.com", "--select", "h1", "--quiet"]);
    assert!(
        out.status.success(),
        "bouncy fetch failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = body.lines().collect();
    assert!(
        lines.iter().any(|l| l.trim() == "Example Domain"),
        "expected to see 'Example Domain' in --select h1 output, got: {body}"
    );
}

#[test]
fn user_agent_default_is_visible_to_httpbin() {
    let out = run(&["fetch", "https://httpbin.org/user-agent", "--quiet"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = String::from_utf8_lossy(&out.stdout);
    // httpbin echoes the UA inside a JSON `user-agent` field.
    assert!(
        body.contains("bouncy/"),
        "expected default UA `bouncy/...` in httpbin response, got: {body}"
    );
}

#[test]
fn user_agent_override_is_propagated_to_httpbin() {
    let out = run(&[
        "fetch",
        "https://httpbin.org/user-agent",
        "--user-agent",
        "e2e-test/1.0",
        "--quiet",
    ]);
    assert!(out.status.success());
    let body = String::from_utf8_lossy(&out.stdout);
    assert!(
        body.contains("e2e-test/1.0"),
        "expected custom UA in httpbin response, got: {body}"
    );
}
