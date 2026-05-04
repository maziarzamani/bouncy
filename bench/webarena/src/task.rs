//! Task definitions.
//!
//! WebArena ships its task suite as JSON files — one document per
//! task with a starting URL, an instruction, and a verification
//! rubric. We mirror only the fields the harness actually consumes
//! today; extending later is additive.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Stable identifier — used for logging and the leaderboard
    /// submission's per-task breakdown.
    pub id: String,
    /// Where the agent starts. Either the URL of a dockerized
    /// WebArena fixture or any reachable site for a smoke test.
    pub start_url: String,
    /// Plain-text instruction handed to the LLM. WebArena's task
    /// schema calls this `intent`; we keep the friendlier name.
    pub instruction: String,
    /// Optional success criteria for the judge. Today's impl checks
    /// substring membership in the model's `done` answer; richer
    /// rubrics (URL match, page-state match, etc.) plug in later.
    #[serde(default)]
    pub expected: Vec<String>,
    /// Cap on agent loop iterations. Defaults to 20 — generous for
    /// most flows, but small enough that a runaway model doesn't
    /// burn unbounded API calls. Override per-task when needed.
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
}

fn default_max_steps() -> u32 {
    20
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_round_trips_through_json() {
        let t = Task {
            id: "t1".into(),
            start_url: "https://x.test/".into(),
            instruction: "click sign up".into(),
            expected: vec!["welcome".into()],
            max_steps: 5,
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: Task = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, "t1");
        assert_eq!(back.max_steps, 5);
        assert_eq!(back.expected, vec!["welcome".to_string()]);
    }

    #[test]
    fn max_steps_defaults_when_absent() {
        let s = r#"{"id":"t","start_url":"http://x/","instruction":"go"}"#;
        let t: Task = serde_json::from_str(s).unwrap();
        assert_eq!(t.max_steps, 20);
        assert!(t.expected.is_empty());
    }
}
