//! End-to-end smoke test for the agent loop.
//!
//! Spins up the in-process fixture, scripts the LLM with a fixed
//! sequence of tool calls, runs the agent, and verifies the
//! trajectory + judge come out as expected. Hermetic — no API
//! credentials, no network beyond `127.0.0.1`.

use std::sync::Arc;

use bouncy_bench_webarena::agent::{run_task, StopReason};
use bouncy_bench_webarena::fixture::spawn_router;
use bouncy_bench_webarena::judge::{Judge, SubstringJudge};
use bouncy_bench_webarena::llm::{AssistantTurn, LlmClient, ScriptedClient};
use bouncy_bench_webarena::task::Task;
use bouncy_bench_webarena::tools::ToolCall;
use bouncy_browse::BrowseOpts;
use serde_json::json;

const LANDING: &str = r#"<!doctype html>
<html><head><title>Landing</title></head>
<body>
  <h1>Welcome</h1>
  <a href="/details">Details</a>
</body></html>"#;

const DETAILS: &str = r#"<!doctype html>
<html><head><title>Details</title></head>
<body>
  <h1>Order #42 — Total $137.50</h1>
  <p>Confirmation: ABC-123</p>
</body></html>"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_clicks_details_link_then_reports_total_via_done() {
    let addr = spawn_router(vec![("/", LANDING), ("/details", DETAILS)]).await;
    let task = Task {
        id: "find_total".into(),
        start_url: format!("http://{addr}/"),
        instruction: "Find the order total on the details page and report it.".into(),
        expected: vec!["137.50".into()],
        max_steps: 6,
    };

    let llm: Arc<dyn LlmClient> = Arc::new(ScriptedClient::new(vec![
        // Turn 1: model navigates to /details by clicking "Details".
        AssistantTurn {
            text: "I'll click the Details link.".into(),
            calls: vec![(
                "call_1".into(),
                ToolCall {
                    name: "click_text".into(),
                    input: json!({"text": "Details"}),
                },
            )],
        },
        // Turn 2: model emits done with the total it read.
        AssistantTurn {
            text: "Found the total.".into(),
            calls: vec![(
                "call_2".into(),
                ToolCall {
                    name: "done".into(),
                    input: json!({"answer": "Order total is $137.50."}),
                },
            )],
        },
    ]));

    let traj = run_task(&task, llm, BrowseOpts::default()).await.unwrap();
    assert_eq!(traj.stop_reason, StopReason::Done);
    assert!(
        traj.answer.as_deref().unwrap_or("").contains("137.50"),
        "got: {:?}",
        traj.answer
    );
    assert_eq!(traj.steps.len(), 2);

    let v = SubstringJudge.score(&task, &traj);
    assert!(v.success, "judge fail: {}", v.reason);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_terminates_on_max_steps_when_model_keeps_acting() {
    let addr = spawn_router(vec![("/", LANDING)]).await;
    let task = Task {
        id: "loop_forever".into(),
        start_url: format!("http://{addr}/"),
        instruction: "(impossible) — model will burn steps".into(),
        expected: vec![],
        max_steps: 2,
    };

    // Three click_text turns — agent caps at max_steps=2 and stops.
    let make_click = |id: &str| AssistantTurn {
        text: "still clicking".into(),
        calls: vec![(
            id.into(),
            ToolCall {
                name: "click_text".into(),
                input: json!({"text": "Welcome"}),
            },
        )],
    };
    let llm: Arc<dyn LlmClient> = Arc::new(ScriptedClient::new(vec![
        make_click("c1"),
        make_click("c2"),
        make_click("c3"),
    ]));

    let traj = run_task(&task, llm, BrowseOpts::default()).await.unwrap();
    assert_eq!(traj.stop_reason, StopReason::MaxSteps);
    assert!(traj.answer.is_none());
    assert_eq!(traj.steps.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_records_failed_tool_calls_without_aborting() {
    // The model first asks for a selector that doesn't exist,
    // then recovers and emits done. The trajectory should record
    // both steps; the judge should pass on the final answer.
    let addr = spawn_router(vec![("/", LANDING)]).await;
    let task = Task {
        id: "recover_from_no_match".into(),
        start_url: format!("http://{addr}/"),
        instruction: "report what page you're on".into(),
        expected: vec!["welcome".into()],
        max_steps: 4,
    };
    let llm: Arc<dyn LlmClient> = Arc::new(ScriptedClient::new(vec![
        AssistantTurn {
            text: "trying a bogus click".into(),
            calls: vec![(
                "c1".into(),
                ToolCall {
                    name: "click".into(),
                    input: json!({"selector": ".does-not-exist"}),
                },
            )],
        },
        AssistantTurn {
            text: "ok, reporting".into(),
            calls: vec![(
                "c2".into(),
                ToolCall {
                    name: "done".into(),
                    input: json!({"answer": "I'm on the Welcome page."}),
                },
            )],
        },
    ]));

    let traj = run_task(&task, llm, BrowseOpts::default()).await.unwrap();
    assert_eq!(traj.stop_reason, StopReason::Done);
    assert_eq!(traj.steps.len(), 2);
    assert!(
        traj.steps[0].is_error,
        "first step should be marked errored"
    );
    assert!(!traj.steps[1].is_error);
    let v = SubstringJudge.score(&task, &traj);
    assert!(v.success, "{}", v.reason);
}
