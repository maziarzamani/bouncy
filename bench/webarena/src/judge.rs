//! Trajectory scoring.
//!
//! Today's `Judge` is a substring-match on the model's `done`
//! answer: every entry in `task.expected` must appear in the
//! answer (case-insensitive, trimmed). It's enough for smoke
//! tests and for a first leaderboard submission against tasks
//! whose verification fits this shape (Q&A, fact lookup, "what's
//! the price of X").
//!
//! Real WebArena tasks use a richer rubric (URL match, expected
//! page state, exact-string vs must-include vs fuzzy-match). The
//! [`Judge`] trait is the seam where that plugs in.

use crate::agent::{StopReason, Trajectory};
use crate::task::Task;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// `true` when every `task.expected` entry was satisfied.
    pub success: bool,
    /// Human-readable explanation. Empty on success.
    pub reason: String,
}

pub trait Judge {
    fn score(&self, task: &Task, traj: &Trajectory) -> Verdict;
}

/// The default judge — case-insensitive substring containment.
pub struct SubstringJudge;

impl Judge for SubstringJudge {
    fn score(&self, task: &Task, traj: &Trajectory) -> Verdict {
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
            Some(a) => a.to_ascii_lowercase(),
            None => {
                return Verdict {
                    success: false,
                    reason: "no answer captured".into(),
                };
            }
        };
        let mut missing: Vec<&str> = Vec::new();
        for needle in &task.expected {
            if !answer.contains(&needle.to_ascii_lowercase()) {
                missing.push(needle);
            }
        }
        if missing.is_empty() {
            Verdict {
                success: true,
                reason: String::new(),
            }
        } else {
            Verdict {
                success: false,
                reason: format!("missing expected fragments: {:?}", missing),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn task(expected: &[&str]) -> Task {
        Task {
            id: "t".into(),
            start_url: "http://x/".into(),
            instruction: "go".into(),
            expected: expected.iter().map(|s| s.to_string()).collect(),
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

    #[test]
    fn judge_passes_when_all_fragments_present() {
        let v = SubstringJudge.score(&task(&["welcome", "alice"]), &done_traj("Welcome, Alice!"));
        assert!(v.success, "got: {:?}", v);
    }

    #[test]
    fn judge_fails_listing_missing_fragments() {
        let v = SubstringJudge.score(&task(&["welcome", "bob"]), &done_traj("Welcome, Alice!"));
        assert!(!v.success);
        assert!(v.reason.contains("bob"), "got: {}", v.reason);
    }

    #[test]
    fn judge_passes_with_no_expected_when_done() {
        let v = SubstringJudge.score(&task(&[]), &done_traj("anything"));
        assert!(v.success);
    }

    #[test]
    fn judge_fails_when_trajectory_didnt_terminate_via_done() {
        let traj = Trajectory {
            stop_reason: StopReason::MaxSteps,
            ..done_traj("doesn't matter")
        };
        let v = SubstringJudge.score(&task(&["x"]), &traj);
        assert!(!v.success);
        assert!(v.reason.contains("did not terminate"), "got: {}", v.reason);
    }
}
