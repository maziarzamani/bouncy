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
use clap::{Parser, ValueEnum};

use bouncy_bench_webarena::agent::{run_task, Trajectory};
use bouncy_bench_webarena::judge::{Judge, SubstringJudge};
use bouncy_bench_webarena::llm::{AnthropicClient, LlmClient};
use bouncy_bench_webarena::task::Task;
use bouncy_bench_webarena::webarena::{
    self, url_map_from_env, UrlMap, WebArenaConfig, WebArenaJudge,
};

#[derive(Parser, Debug)]
#[command(
    name = "bouncy-bench-webarena",
    about = "Agent-loop harness driving bouncy + Claude through WebArena-shaped tasks."
)]
struct Args {
    /// Path to a JSON file containing one task object or an array of
    /// tasks in the harness's simple format. Mutually exclusive with
    /// `--webarena-tasks`.
    #[arg(long, conflicts_with = "webarena_tasks")]
    tasks: Option<PathBuf>,

    /// Path to a directory of WebArena task JSON files (the kind
    /// shipped under `config_files/<id>.json` in
    /// <https://github.com/web-arena-x/webarena>), or a single
    /// `.json` file containing one task. Tasks are scored with the
    /// `WebArenaJudge` (string_match: exact / must_include /
    /// fuzzy_match), and `__SHOPPING__` / `__REDDIT__` etc. URL
    /// placeholders are resolved from env vars (`SHOPPING`,
    /// `REDDIT`, …), matching WebArena's own convention.
    #[arg(long, conflicts_with = "tasks")]
    webarena_tasks: Option<PathBuf>,

    /// LLM backend. `anthropic` is the direct Messages API
    /// (`ANTHROPIC_API_KEY`). `bedrock` is AWS Bedrock's Converse
    /// API (standard AWS credential chain). Bedrock support
    /// requires the `bedrock` cargo feature — build with
    /// `--features bedrock` if you plan to use it.
    #[arg(long, value_enum, default_value_t = Provider::Anthropic)]
    provider: Provider,

    /// Model id. For `anthropic`: a bare Anthropic id like
    /// `claude-sonnet-4-6`. For `bedrock`: a Bedrock model id like
    /// `anthropic.claude-sonnet-4-5-20250929-v1:0` or an inference
    /// profile ARN. Defaults to the Anthropic Sonnet id; override
    /// when targeting Bedrock.
    #[arg(long, default_value = "claude-sonnet-4-6")]
    model: String,

    /// AWS region for `--provider bedrock`. Falls back to
    /// `AWS_REGION` / `AWS_DEFAULT_REGION` env, then the SDK's
    /// usual default chain.
    #[arg(long)]
    region: Option<String>,

    /// Stealth fingerprinting on the bouncy session.
    #[arg(long)]
    stealth: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Provider {
    Anthropic,
    Bedrock,
}

/// One task ready to run plus the judge that scores its trajectory.
/// Lets us mix simple-format tasks (substring judge) and WebArena
/// tasks (rubric judge) in the same loop.
struct Job {
    task: Task,
    judge: Box<dyn Judge>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    bouncy_bench_webarena::install_crypto_provider();
    let args = Args::parse();
    eprintln!(
        "bouncy-bench-webarena — provider={:?}, model={}",
        args.provider, args.model
    );

    let jobs = load_jobs(&args)?;
    eprintln!("loaded {} task(s)", jobs.len());
    eprintln!("building LLM client …");
    let llm: Arc<dyn LlmClient> = build_client(&args).await?;
    eprintln!("LLM client ready");

    let mut summary = Vec::with_capacity(jobs.len());
    for job in &jobs {
        eprintln!("→ task {}: {}", job.task.id, job.task.instruction);
        let opts = BrowseOpts {
            stealth: args.stealth,
            ..BrowseOpts::default()
        };
        let traj = match run_task(&job.task, llm.clone(), opts).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("  ✗ harness error: {e}");
                summary.push((job.task.id.clone(), false, 0u64, format!("error: {e}")));
                continue;
            }
        };
        let verdict = job.judge.score(&job.task, &traj);
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
        summary.push((
            job.task.id.clone(),
            verdict.success,
            elapsed_ms,
            verdict.reason,
        ));
    }
    print_summary(&summary, jobs.len());
    Ok(())
}

/// Resolve the `--tasks` / `--webarena-tasks` flags into a uniform
/// list of [`Job`]s. Errors loudly when neither flag is given so
/// the user gets a clear "what do I run?" message instead of an
/// empty success.
fn load_jobs(args: &Args) -> Result<Vec<Job>> {
    if let Some(path) = &args.webarena_tasks {
        let url_map = url_map_from_env();
        if url_map.is_empty() {
            eprintln!(
                "  warn: no WebArena URL placeholders in env (SHOPPING / REDDIT / GITLAB / MAP / SHOPPING_ADMIN / WIKIPEDIA / HOMEPAGE); tasks with __PLACEHOLDER__ start_urls will fail to load"
            );
        } else {
            let keys: Vec<&String> = url_map.keys().collect();
            eprintln!("  WebArena URL map: {keys:?}");
        }
        return load_webarena_jobs(path, &url_map);
    }
    if let Some(path) = &args.tasks {
        return load_simple_jobs(path);
    }
    Err(anyhow::anyhow!(
        "specify --tasks <FILE> (simple format) or --webarena-tasks <FILE-OR-DIR> (WebArena format)"
    ))
}

fn load_simple_jobs(path: &PathBuf) -> Result<Vec<Job>> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read tasks file {path:?}"))?;
    let tasks = parse_tasks(&raw)?;
    Ok(tasks
        .into_iter()
        .map(|task| Job {
            task,
            judge: Box::new(SubstringJudge),
        })
        .collect())
}

fn load_webarena_jobs(path: &PathBuf, url_map: &UrlMap) -> Result<Vec<Job>> {
    const DEFAULT_MAX_STEPS: u32 = 30;
    let configs = read_webarena_configs(path)?;
    let mut jobs = Vec::with_capacity(configs.len());
    for cfg in configs {
        let task = webarena::to_task(&cfg, url_map, DEFAULT_MAX_STEPS)
            .map_err(|e| anyhow::anyhow!("task {}: {e}", cfg.task_id))?;
        let judge = WebArenaJudge { eval: cfg.eval };
        jobs.push(Job {
            task,
            judge: Box::new(judge),
        });
    }
    Ok(jobs)
}

/// Read one or many WebArena task JSON files. `path` is either:
///   - a single `.json` file → one task
///   - a directory → every `*.json` inside, sorted by file name
fn read_webarena_configs(path: &PathBuf) -> Result<Vec<WebArenaConfig>> {
    let meta = std::fs::metadata(path).with_context(|| format!("stat {path:?}"))?;
    if meta.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
            .with_context(|| format!("read_dir {path:?}"))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        entries.sort();
        let mut out = Vec::with_capacity(entries.len());
        for p in entries {
            let raw = std::fs::read_to_string(&p).with_context(|| format!("read {p:?}"))?;
            // Each file may be a single task or an array (WebArena's
            // own `test.raw.json` is an array).
            push_webarena_from_str(&raw, &mut out).with_context(|| format!("parse {p:?}"))?;
        }
        Ok(out)
    } else {
        let raw = std::fs::read_to_string(path).with_context(|| format!("read {path:?}"))?;
        let mut out = Vec::new();
        push_webarena_from_str(&raw, &mut out).with_context(|| format!("parse {path:?}"))?;
        Ok(out)
    }
}

fn push_webarena_from_str(raw: &str, out: &mut Vec<WebArenaConfig>) -> Result<()> {
    if raw.trim_start().starts_with('[') {
        let arr: Vec<WebArenaConfig> = serde_json::from_str(raw)?;
        out.extend(arr);
    } else {
        out.push(serde_json::from_str(raw)?);
    }
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

/// Build an [`LlmClient`] for the chosen provider. Bedrock is
/// behind the `bedrock` cargo feature so users who only need the
/// Anthropic-direct path don't pay the AWS SDK's compile cost.
async fn build_client(args: &Args) -> Result<Arc<dyn LlmClient>> {
    match args.provider {
        Provider::Anthropic => Ok(Arc::new(AnthropicClient::from_env(&args.model)?)),
        Provider::Bedrock => bedrock_client(args).await,
    }
}

#[cfg(feature = "bedrock")]
async fn bedrock_client(args: &Args) -> Result<Arc<dyn LlmClient>> {
    use bouncy_bench_webarena::llm::BedrockClient;
    let client = BedrockClient::from_env(&args.model, args.region.clone()).await?;
    Ok(Arc::new(client))
}

#[cfg(not(feature = "bedrock"))]
async fn bedrock_client(_args: &Args) -> Result<Arc<dyn LlmClient>> {
    Err(anyhow::anyhow!(
        "this binary was built without the `bedrock` feature — rebuild with `--features bedrock` to use AWS Bedrock"
    ))
}

fn print_summary(summary: &[(String, bool, u64, String)], total: usize) {
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
