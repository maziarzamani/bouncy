//! End-to-end smoke test — spawns the bouncy-mcp binary, drives it over
//! stdio with raw MCP JSON-RPC messages, and verifies the protocol
//! handshake + the `extract_title` tool round-trip.
//!
//! Hermetic: the `extract_title` tool is offline, so this test never
//! touches the network. Slower than unit tests because it cargo-builds
//! the binary, hence `cargo test` runs it like any other test but it
//! lives behind the dev-dependencies build.

use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_bouncy-mcp");

async fn write_msg(stdin: &mut tokio::process::ChildStdin, v: Value) {
    let line = serde_json::to_string(&v).unwrap();
    stdin.write_all(line.as_bytes()).await.unwrap();
    stdin.write_all(b"\n").await.unwrap();
    stdin.flush().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn handshake_lists_tools_and_calls_extract_title() {
    let mut child = Command::new(BIN)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bouncy-mcp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");

    // Write all four messages up-front and close stdin. The rmcp stdio
    // transport reads in buffered chunks; closing the input side forces
    // the server to drain and reply to the requests it has seen.
    write_msg(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "bouncy-mcp-smoke", "version": "0"}
            }
        }),
    )
    .await;
    write_msg(
        &mut stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    write_msg(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await;
    write_msg(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "extract_title",
                "arguments": {
                    "html": "<html><head><title>hello world</title></head><body></body></html>"
                }
            }
        }),
    )
    .await;
    drop(stdin);

    let mut reader = BufReader::new(stdout).lines();
    let mut by_id = std::collections::HashMap::<u64, Value>::new();
    while let Some(line) = tokio::time::timeout(Duration::from_secs(10), reader.next_line())
        .await
        .expect("read timeout (10s)")
        .expect("io error")
    {
        if line.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(&line).expect("invalid JSON");
        if let Some(id) = v.get("id").and_then(|x| x.as_u64()) {
            by_id.insert(id, v);
        }
    }

    let init = by_id.get(&1).expect("no initialize response");
    assert!(
        init["result"]["serverInfo"].is_object(),
        "init response missing serverInfo: {init}"
    );

    let list = by_id.get(&2).expect("no tools/list response");
    let tools = list["result"]["tools"]
        .as_array()
        .expect("tools array missing");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    for expected in [
        "fetch",
        "extract_title",
        "extract_text",
        "extract_links",
        "js_eval",
        "scrape",
        "scrape_many",
    ] {
        assert!(
            names.contains(&expected),
            "tool '{expected}' not advertised; got {names:?}"
        );
    }

    let call = by_id.get(&3).expect("no tools/call response");
    let content = call["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no content[0].text in {call}"));
    assert!(
        content.contains("hello world"),
        "expected title text in content; got: {content}"
    );

    let _ = child.wait().await;
}
