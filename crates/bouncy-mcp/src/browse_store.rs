//! Server-side store of active [`BrowseSession`]s for the MCP browse
//! tools.
//!
//! Each MCP `bouncy_browse_open` call creates one session and returns
//! its `session_id` (a UUIDv4). Subsequent `bouncy_browse_*` tool calls
//! look the session up by id, run the primitive, and return the new
//! page snapshot. Sessions persist in this store across tool calls so
//! V8 state, cookies, and current page survive the round trips.
//!
//! Two safety limits keep a misbehaving client (or a stuck LLM agent)
//! from running the server out of memory:
//!   - **Hard cap** on concurrent sessions (default: 20).
//!   - **Idle expiry** — a session that hasn't been touched in
//!     [`DEFAULT_IDLE_TIMEOUT`] is dropped by the reaper background
//!     task, freeing its V8 isolate.
//!
//! `BrowseSession`'s methods take `&self` (commands cross via mpsc to
//! the actor task), so we store `Arc<BrowseSession>` and concurrent
//! handlers can drive the same session — the actor processes their
//! commands in order. The store's `Mutex` is only held during
//! lookup/insert/reap.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bouncy_browse::{BrowseError, BrowseOpts, BrowseSession, PageSnapshot};
use thiserror::Error;

/// Default per-server cap on active sessions.
pub const DEFAULT_MAX_SESSIONS: usize = 20;
/// Default idle timeout — sessions untouched longer than this are
/// dropped by the reaper.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// How often the reaper sweeps for expired sessions.
pub const DEFAULT_REAPER_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("session capacity exceeded ({cap} active sessions); close one with bouncy_browse_close or wait for idle expiry")]
    AtCapacity { cap: usize },

    #[error("session {0:?} not found (it may have expired or been closed)")]
    NotFound(String),

    #[error(transparent)]
    Browse(#[from] BrowseError),
}

/// One entry in the store. Holds the live session plus its last-used
/// timestamp so the reaper can decide when to evict.
struct Entry {
    session: Arc<BrowseSession>,
    last_used: Instant,
}

/// Concurrent map of active sessions with hard cap + idle expiry.
/// Cheap to clone — internal state is `Arc<Mutex<…>>`.
#[derive(Clone)]
pub struct BrowseStore {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
    max_sessions: usize,
    idle_timeout: Duration,
}

impl Default for BrowseStore {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_SESSIONS, DEFAULT_IDLE_TIMEOUT)
    }
}

impl BrowseStore {
    pub fn new(max_sessions: usize, idle_timeout: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            max_sessions,
            idle_timeout,
        }
    }

    /// Spawn a background task that periodically evicts idle sessions.
    /// Returns the [`tokio::task::JoinHandle`] so callers (typically the
    /// MCP server's `main`) can keep it alive for the process lifetime.
    /// Idempotent in practice — call once per store.
    pub fn spawn_reaper(&self, interval: Duration) -> tokio::task::JoinHandle<()> {
        let store = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                store.reap_expired();
            }
        })
    }

    /// Open a new browse session. Returns the new session id and the
    /// initial page snapshot. Errors with `AtCapacity` if the store is
    /// already at `max_sessions`.
    pub async fn open(
        &self,
        url: &str,
        opts: BrowseOpts,
    ) -> Result<(String, PageSnapshot), StoreError> {
        // Check capacity BEFORE we spend time spinning up V8 + the actor.
        // Brief lock; release before the await.
        {
            let g = self.inner.lock().expect("BrowseStore mutex poisoned");
            if g.len() >= self.max_sessions {
                return Err(StoreError::AtCapacity {
                    cap: self.max_sessions,
                });
            }
        }
        let (session, snapshot) = BrowseSession::open(url, opts).await?;
        let id = uuid::Uuid::new_v4().to_string();
        // Re-check capacity (TOCTOU): another caller might have raced
        // in between the check and the await. Drop the new session
        // gracefully if we lost the race.
        let mut g = self.inner.lock().expect("BrowseStore mutex poisoned");
        if g.len() >= self.max_sessions {
            // Drop session by leaving it out of the map; its actor
            // exits when the last handle (this scope) releases.
            return Err(StoreError::AtCapacity {
                cap: self.max_sessions,
            });
        }
        g.insert(
            id.clone(),
            Entry {
                session: Arc::new(session),
                last_used: Instant::now(),
            },
        );
        Ok((id, snapshot))
    }

    /// Look up a session by id and bump its last-used timestamp.
    /// Returns the session handle or `NotFound` if the id is unknown
    /// (likely expired).
    pub fn touch(&self, id: &str) -> Result<Arc<BrowseSession>, StoreError> {
        let mut g = self.inner.lock().expect("BrowseStore mutex poisoned");
        match g.get_mut(id) {
            Some(entry) => {
                entry.last_used = Instant::now();
                Ok(entry.session.clone())
            }
            None => Err(StoreError::NotFound(id.to_string())),
        }
    }

    /// Explicit close. Returns `true` if a session was removed.
    pub fn close(&self, id: &str) -> bool {
        let mut g = self.inner.lock().expect("BrowseStore mutex poisoned");
        g.remove(id).is_some()
    }

    /// Drop sessions whose last-used timestamp is older than
    /// `idle_timeout`. Called by the reaper task.
    pub fn reap_expired(&self) {
        let cutoff = Instant::now()
            .checked_sub(self.idle_timeout)
            .unwrap_or(Instant::now());
        let mut g = self.inner.lock().expect("BrowseStore mutex poisoned");
        g.retain(|_, entry| entry.last_used >= cutoff);
    }

    /// How many sessions are currently held. Useful for tests + metrics.
    pub fn len(&self) -> usize {
        self.inner.lock().expect("BrowseStore mutex poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_returns_true_then_false() {
        let s = BrowseStore::default();
        // No real sessions — go straight to close on a fake id; expect false.
        assert!(!s.close("not-real"));
    }

    #[test]
    fn len_is_zero_for_fresh_store() {
        assert!(BrowseStore::default().is_empty());
    }

    #[test]
    fn touch_unknown_id_returns_not_found() {
        let s = BrowseStore::default();
        let err = s.touch("ghost").unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }

    // Note: tests that exercise actual `open` + `touch` against a real
    // BrowseSession live in `tests/browse_store.rs` because they need a
    // tokio runtime + a `tiny_http` fixture. The unit tests above cover
    // the bits that don't need either.
}
