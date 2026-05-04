//! WebArena task-format support.
//!
//! Wraps the JSON config shape WebArena ships in `config_files/`
//! (one document per task) so the harness can run real WebArena
//! tasks against locally-running fixture containers.
//!
//! Schema reference: <https://github.com/web-arena-x/webarena/blob/main/config_files/test.raw.json>
//!
//! Coverage today:
//!
//!   - `string_match` eval — `exact_match`, `must_include`,
//!     `fuzzy_match` (substring approximation; WebArena's official
//!     fuzzy eval uses an LLM judge — documented gap).
//!   - URL templating: `__SHOPPING__` / `__SHOPPING_ADMIN__` /
//!     `__REDDIT__` / `__GITLAB__` / `__MAP__` / `__WIKIPEDIA__`
//!     placeholders are replaced from a [`UrlMap`] before the
//!     session opens. Map entries follow WebArena's env-var
//!     convention so an existing WebArena setup reuses its
//!     `$SHOPPING` etc. directly.
//!   - `url_match` — surfaces a typed-but-not-implemented error
//!     because the trajectory doesn't yet thread the final URL.
//!     Closing that gap is mechanical (one field on
//!     [`crate::agent::Trajectory`]); it's deferred until needed.
//!   - `program_html` — same: typed gap, not implemented.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::agent::{StopReason, Trajectory};
use crate::judge::{Judge, Verdict};
use crate::task::Task;

/// One WebArena task, parsed from a `config_files/<id>.json` file.
/// Many fields are present in the upstream schema; we mirror only
/// the ones the harness consumes today. Extra fields are silently
/// ignored (serde default behaviour).
#[derive(Debug, Clone, Deserialize)]
pub struct WebArenaConfig {
    pub task_id: i64,
    #[serde(default)]
    pub sites: Vec<String>,
    pub start_url: String,
    pub intent: String,
    pub eval: WebArenaEval,
    /// Optional cap; falls back to harness default when absent.
    #[serde(default)]
    pub max_steps: Option<u32>,
}

/// The eval rubric. Real WebArena tasks combine multiple eval
/// types — e.g. `["string_match", "url_match"]` for tasks that
/// expect both an answer and a final URL.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WebArenaEval {
    #[serde(default)]
    pub eval_types: Vec<String>,
    #[serde(default)]
    pub reference_answers: Option<ReferenceAnswers>,
    #[serde(default)]
    pub reference_url: Option<String>,
}

/// String-match reference values. WebArena tasks usually populate
/// at most one of these per task; the judge enforces all of them
/// when more than one is present.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReferenceAnswers {
    #[serde(default)]
    pub exact_match: Option<String>,
    #[serde(default)]
    pub must_include: Vec<String>,
    #[serde(default)]
    pub fuzzy_match: Vec<String>,
}

/// Map of placeholder → real URL. Keys include the underscores
/// (`"__SHOPPING__"`); values are the running fixture URLs
/// (`"http://localhost:7770"`).
pub type UrlMap = BTreeMap<String, String>;

/// Build a [`UrlMap`] from environment variables that match
/// WebArena's own naming (`SHOPPING`, `SHOPPING_ADMIN`, `MAP`,
/// `REDDIT`, `GITLAB`, `WIKIPEDIA`). Missing vars are silently
/// skipped — tasks that need them will fail with a clear
/// "no such placeholder" error from [`apply_url_map`] later.
pub fn url_map_from_env() -> UrlMap {
    const KNOWN: &[&str] = &[
        "SHOPPING",
        "SHOPPING_ADMIN",
        "MAP",
        "REDDIT",
        "GITLAB",
        "WIKIPEDIA",
        "HOMEPAGE",
    ];
    let mut map = UrlMap::new();
    for var in KNOWN {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                map.insert(format!("__{}__", var), v);
            }
        }
    }
    map
}

/// Replace every `__PLACEHOLDER__` occurrence in `url` with its
/// mapped value. Returns an error listing any placeholders that
/// weren't in the map — much friendlier than letting the agent
/// open `https://__SHOPPING__/...` and fail with a DNS error.
pub fn apply_url_map(url: &str, map: &UrlMap) -> Result<String, String> {
    let mut out = url.to_string();
    for (k, v) in map {
        out = out.replace(k, v);
    }
    let mut missing: Vec<String> = Vec::new();
    let mut search = out.as_str();
    while let Some(start) = search.find("__") {
        if let Some(end_rel) = search[start + 2..].find("__") {
            let end = start + 2 + end_rel + 2;
            let token = &search[start..end];
            if !token[2..token.len() - 2].is_empty() {
                missing.push(token.to_string());
            }
            search = &search[end..];
        } else {
            break;
        }
    }
    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        return Err(format!(
            "url has unresolved placeholders: {missing:?}; populate them via env vars or a CLI map"
        ));
    }
    Ok(out)
}

/// Convert a parsed [`WebArenaConfig`] + URL map into a [`Task`]
/// the harness's `run_task` understands. Non-fatal note: WebArena
/// tasks don't ship `expected[]` (the eval is richer); the
/// resulting `Task.expected` is empty and the substring judge
/// would always pass — pair this conversion with [`WebArenaJudge`]
/// to actually score.
pub fn to_task(
    cfg: &WebArenaConfig,
    url_map: &UrlMap,
    default_max_steps: u32,
) -> Result<Task, String> {
    let start_url = apply_url_map(&cfg.start_url, url_map)?;
    Ok(Task {
        id: cfg.task_id.to_string(),
        start_url,
        instruction: cfg.intent.clone(),
        expected: Vec::new(),
        max_steps: cfg.max_steps.unwrap_or(default_max_steps),
    })
}

/// Score a trajectory against a [`WebArenaEval`]. One judge per
/// task (the rubric is per-task), so callers typically wrap a
/// `WebArenaConfig` and its judge as a pair.
pub struct WebArenaJudge {
    pub eval: WebArenaEval,
}

impl Judge for WebArenaJudge {
    fn score(&self, _task: &Task, traj: &Trajectory) -> Verdict {
        if traj.stop_reason != StopReason::Done {
            return Verdict {
                success: false,
                reason: format!(
                    "trajectory did not terminate via done: {:?}",
                    traj.stop_reason
                ),
            };
        }
        let answer = match traj.answer.as_deref() {
            Some(a) => a.trim().to_string(),
            None => {
                return Verdict {
                    success: false,
                    reason: "no answer captured".into(),
                };
            }
        };
        let answer_lower = answer.to_lowercase();

        for eval_type in &self.eval.eval_types {
            match eval_type.as_str() {
                "string_match" => {
                    if let Err(reason) = check_string_match(&self.eval, &answer, &answer_lower) {
                        return Verdict {
                            success: false,
                            reason,
                        };
                    }
                }
                "url_match" => {
                    return Verdict {
                        success: false,
                        reason: "url_match eval not yet supported — Trajectory needs a final_url field; track in a follow-up issue".into(),
                    };
                }
                "program_html" => {
                    return Verdict {
                        success: false,
                        reason: "program_html eval not yet supported — needs a post-task DOM fetch + lxml-style locator port".into(),
                    };
                }
                other => {
                    return Verdict {
                        success: false,
                        reason: format!("unknown eval type: {other:?}"),
                    };
                }
            }
        }
        // No eval_types declared at all → treat as success when the
        // model emitted `done`. Permissive on purpose; explicit
        // mismatch is louder than silent skip.
        Verdict {
            success: true,
            reason: String::new(),
        }
    }
}

fn check_string_match(eval: &WebArenaEval, answer: &str, answer_lower: &str) -> Result<(), String> {
    let refs = match &eval.reference_answers {
        Some(r) => r,
        None => {
            // Declared eval_type but no reference_answers — the task
            // file is malformed. Fail loudly so it's caught early.
            return Err("string_match declared without reference_answers".into());
        }
    };
    if let Some(expected) = &refs.exact_match {
        let want = expected.trim().to_lowercase();
        if answer_lower != want {
            return Err(format!("exact_match expected {expected:?}, got {answer:?}"));
        }
    }
    let mut missing: Vec<&str> = Vec::new();
    for needle in &refs.must_include {
        if !answer_lower.contains(&needle.to_lowercase()) {
            missing.push(needle);
        }
    }
    if !missing.is_empty() {
        return Err(format!("must_include missing: {missing:?}"));
    }
    // fuzzy_match: WebArena's official eval uses an LLM judge here
    // (semantic equivalence). We approximate with case-insensitive
    // substring containment — same as must_include, but documented
    // separately so callers know it's a substring fallback rather
    // than the upstream semantic check.
    let mut fuzzy_missing: Vec<&str> = Vec::new();
    for needle in &refs.fuzzy_match {
        if !answer_lower.contains(&needle.to_lowercase()) {
            fuzzy_missing.push(needle);
        }
    }
    if !fuzzy_missing.is_empty() {
        return Err(format!(
            "fuzzy_match (substring approximation) missing: {fuzzy_missing:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn task() -> Task {
        Task {
            id: "t".into(),
            start_url: "http://x/".into(),
            instruction: "go".into(),
            expected: vec![],
            max_steps: 5,
        }
    }

    fn done_traj(answer: &str) -> Trajectory {
        Trajectory {
            task_id: "t".into(),
            answer: Some(answer.into()),
            steps: vec![],
            elapsed: Duration::from_millis(1),
            stop_reason: StopReason::Done,
        }
    }

    // ---- URL map ----

    #[test]
    fn apply_url_map_replaces_known_placeholders() {
        let mut m = UrlMap::new();
        m.insert("__SHOPPING__".into(), "http://localhost:7770".into());
        let out = apply_url_map("__SHOPPING__/checkout", &m).unwrap();
        assert_eq!(out, "http://localhost:7770/checkout");
    }

    #[test]
    fn apply_url_map_errors_on_unknown_placeholder() {
        let m = UrlMap::new();
        let err = apply_url_map("__SHOPPING__/x", &m).unwrap_err();
        assert!(err.contains("__SHOPPING__"), "got: {err}");
    }

    #[test]
    fn apply_url_map_passes_through_when_no_placeholders() {
        let m = UrlMap::new();
        assert_eq!(
            apply_url_map("https://example.com/", &m).unwrap(),
            "https://example.com/"
        );
    }

    // ---- WebArenaJudge ----

    #[test]
    fn judge_passes_exact_match_case_insensitive() {
        let eval = WebArenaEval {
            eval_types: vec!["string_match".into()],
            reference_answers: Some(ReferenceAnswers {
                exact_match: Some("$23.99".into()),
                ..Default::default()
            }),
            reference_url: None,
        };
        let v = WebArenaJudge { eval }.score(&task(), &done_traj("  $23.99 "));
        assert!(v.success, "got: {v:?}");
    }

    #[test]
    fn judge_fails_exact_match_mismatch_with_clear_message() {
        let eval = WebArenaEval {
            eval_types: vec!["string_match".into()],
            reference_answers: Some(ReferenceAnswers {
                exact_match: Some("$23.99".into()),
                ..Default::default()
            }),
            reference_url: None,
        };
        let v = WebArenaJudge { eval }.score(&task(), &done_traj("$24.00"));
        assert!(!v.success);
        assert!(v.reason.contains("$23.99"), "got: {}", v.reason);
        assert!(v.reason.contains("$24.00"), "got: {}", v.reason);
    }

    #[test]
    fn judge_must_include_lists_missing_fragments() {
        let eval = WebArenaEval {
            eval_types: vec!["string_match".into()],
            reference_answers: Some(ReferenceAnswers {
                must_include: vec!["alice".into(), "premium".into()],
                ..Default::default()
            }),
            reference_url: None,
        };
        let v = WebArenaJudge { eval }.score(
            &task(),
            &done_traj("Alice signed up but didn't choose a tier"),
        );
        assert!(!v.success);
        assert!(v.reason.contains("premium"), "got: {}", v.reason);
        assert!(!v.reason.contains("alice"), "got: {}", v.reason);
    }

    #[test]
    fn judge_fuzzy_match_uses_substring_approximation() {
        // Real WebArena would call an LLM here; we substring-match
        // with a comment in the docs warning users.
        let eval = WebArenaEval {
            eval_types: vec!["string_match".into()],
            reference_answers: Some(ReferenceAnswers {
                fuzzy_match: vec!["a positive review".into()],
                ..Default::default()
            }),
            reference_url: None,
        };
        let v = WebArenaJudge { eval }.score(
            &task(),
            &done_traj("This is a positive review of the item."),
        );
        assert!(v.success, "got: {v:?}");
    }

    #[test]
    fn judge_unknown_eval_type_fails_loudly() {
        let eval = WebArenaEval {
            eval_types: vec!["bonkers_match".into()],
            reference_answers: None,
            reference_url: None,
        };
        let v = WebArenaJudge { eval }.score(&task(), &done_traj("anything"));
        assert!(!v.success);
        assert!(v.reason.contains("bonkers_match"), "got: {}", v.reason);
    }

    #[test]
    fn judge_url_match_returns_typed_not_implemented() {
        let eval = WebArenaEval {
            eval_types: vec!["url_match".into()],
            reference_answers: None,
            reference_url: Some("http://x/done".into()),
        };
        let v = WebArenaJudge { eval }.score(&task(), &done_traj("anything"));
        assert!(!v.success);
        assert!(v.reason.contains("url_match"), "got: {}", v.reason);
        assert!(v.reason.contains("not yet supported"), "got: {}", v.reason);
    }

    #[test]
    fn judge_passes_when_no_eval_types_and_done() {
        let v = WebArenaJudge {
            eval: WebArenaEval::default(),
        }
        .score(&task(), &done_traj("anything"));
        assert!(v.success, "got: {v:?}");
    }

    #[test]
    fn judge_fails_when_trajectory_didnt_terminate() {
        let traj = Trajectory {
            stop_reason: StopReason::MaxSteps,
            ..done_traj("doesn't matter")
        };
        let v = WebArenaJudge {
            eval: WebArenaEval::default(),
        }
        .score(&task(), &traj);
        assert!(!v.success);
        assert!(v.reason.contains("did not terminate"), "got: {}", v.reason);
    }

    // ---- WebArenaConfig parsing ----

    #[test]
    fn config_parses_a_minimal_task_json() {
        let raw = r#"{
            "task_id": 42,
            "sites": ["shopping"],
            "start_url": "__SHOPPING__/cart",
            "intent": "What's in my cart?",
            "eval": {
                "eval_types": ["string_match"],
                "reference_answers": {"must_include": ["empty"]}
            }
        }"#;
        let cfg: WebArenaConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(cfg.task_id, 42);
        assert_eq!(cfg.start_url, "__SHOPPING__/cart");
        assert_eq!(cfg.eval.eval_types, vec!["string_match".to_string()]);
        let refs = cfg.eval.reference_answers.unwrap();
        assert_eq!(refs.must_include, vec!["empty".to_string()]);
    }

    #[test]
    fn config_to_task_resolves_placeholder() {
        let cfg = WebArenaConfig {
            task_id: 1,
            sites: vec!["shopping".into()],
            start_url: "__SHOPPING__/cart".into(),
            intent: "?".into(),
            eval: WebArenaEval::default(),
            max_steps: None,
        };
        let mut m = UrlMap::new();
        m.insert("__SHOPPING__".into(), "http://localhost:7770".into());
        let t = to_task(&cfg, &m, 30).unwrap();
        assert_eq!(t.id, "1");
        assert_eq!(t.start_url, "http://localhost:7770/cart");
        assert_eq!(t.max_steps, 30);
    }

    #[test]
    fn config_to_task_errors_when_placeholder_missing() {
        let cfg = WebArenaConfig {
            task_id: 1,
            sites: vec![],
            start_url: "__SHOPPING__/x".into(),
            intent: "?".into(),
            eval: WebArenaEval::default(),
            max_steps: None,
        };
        let m = UrlMap::new();
        assert!(to_task(&cfg, &m, 30).is_err());
    }
}
