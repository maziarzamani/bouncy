//! `bouncy-bench-webarena` CLI.
//!
//! Reads a JSON file of tasks (one [`Task`] or an array), runs each
//! against a real Anthropic Messages API session, and prints a
//! summary of success rate + per-task wall-clock.
//!
//!     ANTHROPIC_API_KEY=sk-ant-… \
//!       bouncy-bench-webarena --tasks tasks.json --model claude-sonnet-4-6
//!
//! For tests, see `tests/agent_smoke.rs` — that exercises the same
//! [`run_task`] entry point with a [`ScriptedClient`] so no API
//! credentials are required.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use bouncy_browse::BrowseOpts;
use clap::Parser;

use bouncy_bench_webarena::agent::{run_task, Trajectory};
use bouncy_bench_webarena::judge::{Judge, SubstringJudge};
use bouncy_bench_webarena::llm::AnthropicClient;
use bouncy_bench_webarena::task::Task;

#[derive(Parser, Debug)]
#[command(
    name = "bouncy-bench-webarena",
    about = "Agent-loop harness driving bouncy + Claude through WebArena-shaped tasks."
)]
struct Args {
    /// Path to a JSON file containing one task object or an array
    /// of tasks.
    #[arg(long)]
    tasks: PathBuf,

    /// Anthropic model id. Defaults to a recent Sonnet.
    #[arg(long, default_value = "claude-sonnet-4-6")]
    model: String,

    /// Stealth fingerprinting on the bouncy session.
    #[arg(long)]
    stealth: bool,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let raw = std::fs::read_to_string(&args.tasks)
        .with_context(|| format!("read tasks file {:?}", args.tasks))?;
    let tasks = parse_tasks(&raw)?;
    let llm: Arc<dyn bouncy_bench_webarena::llm::LlmClient> =
        Arc::new(AnthropicClient::from_env(&args.model)?);
    let judge = SubstringJudge;

    let mut summary = Vec::with_capacity(tasks.len());
    for task in &tasks {
        eprintln!("→ task {}: {}", task.id, task.instruction);
        let opts = BrowseOpts {
            stealth: args.stealth,
            ..BrowseOpts::default()
        };
        let traj = match run_task(task, llm.clone(), opts).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("  ✗ harness error: {e}");
                summary.push((task.id.clone(), false, 0u64, format!("error: {e}")));
                continue;
            }
        };
        let verdict = judge.score(task, &traj);
        let elapsed_ms = traj.elapsed.as_millis() as u64;
        eprintln!(
            "  {} ({elapsed_ms} ms, {} steps) — {}",
            if verdict.success { "✓" } else { "✗" },
            traj.steps.len(),
            if verdict.reason.is_empty() {
                "ok".to_string()
            } else {
                verdict.reason.clone()
            }
        );
        summary.push((task.id.clone(), verdict.success, elapsed_ms, verdict.reason));
    }
    print_summary(&summary, &tasks);
    Ok(())
}

fn parse_tasks(raw: &str) -> Result<Vec<Task>> {
    // Accept either a single task object or an array.
    if raw.trim_start().starts_with('[') {
        Ok(serde_json::from_str(raw)?)
    } else {
        Ok(vec![serde_json::from_str(raw)?])
    }
}

fn print_summary(summary: &[(String, bool, u64, String)], tasks: &[Task]) {
    let total = tasks.len();
    let passed = summary.iter().filter(|(_, ok, _, _)| *ok).count();
    let pct = if total == 0 {
        0.0
    } else {
        100.0 * passed as f64 / total as f64
    };
    let total_ms: u64 = summary.iter().map(|(_, _, ms, _)| *ms).sum();
    let median_ms = median_ms(summary);
    eprintln!();
    eprintln!("=== summary ===");
    eprintln!("  passed: {passed}/{total} ({pct:.1}%)");
    eprintln!("  total wall-clock: {total_ms} ms");
    eprintln!("  median per-task wall-clock: {median_ms} ms");
}

fn median_ms(summary: &[(String, bool, u64, String)]) -> u64 {
    if summary.is_empty() {
        return 0;
    }
    let mut times: Vec<u64> = summary.iter().map(|(_, _, ms, _)| *ms).collect();
    times.sort_unstable();
    times[times.len() / 2]
}

// `Trajectory` is used in the public API the binary depends on but
// not directly referenced here — the import keeps the symbol live
// for `cargo doc` and trims a dead-code warning.
#[allow(dead_code)]
fn _public_api_anchor(_t: Trajectory) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tasks_accepts_single_object_or_array() {
        let one = r#"{"id":"a","start_url":"http://x/","instruction":"go"}"#;
        let arr = format!("[{one},{one}]");
        assert_eq!(parse_tasks(one).unwrap().len(), 1);
        assert_eq!(parse_tasks(&arr).unwrap().len(), 2);
    }

    #[test]
    fn median_ms_returns_middle_after_sort() {
        let s = vec![
            ("a".into(), true, 10, String::new()),
            ("b".into(), true, 50, String::new()),
            ("c".into(), false, 30, String::new()),
        ];
        assert_eq!(median_ms(&s), 30);
    }

    #[test]
    fn median_ms_zero_when_empty() {
        assert_eq!(median_ms(&[]), 0);
    }
}
