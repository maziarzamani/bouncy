//! `bouncy scrape` — parallel URL scraper.
//!
//! Per-URL pipeline: fetch (with retries + exponential backoff on
//! transient failures) → optional V8 eval → row collected. Results are
//! buffered then emitted as JSON or tab-separated text.
//!
//! When the optional `tx` channel is `Some`, the task closure also
//! emits per-URL state-transition events for the `--tui` dashboard
//! (see `scrape_tui.rs`). When `tx` is `None`, behavior is identical
//! to the pre-event implementation.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use bouncy_extract::extract_title;
use bouncy_fetch::Fetcher;
use bouncy_js::Runtime;
use futures_util::stream::{self, StreamExt};
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{build_blocklist, load_cookie_jar, save_cookie_jar};

/// Per-URL state-transition events emitted by the scrape task closure.
/// The TUI dashboard subscribes via an mpsc channel; non-TUI runs pass
/// `None` for the sender and these events are never created.
///
/// Some fields (body_size, per-attempt latency, backoff, retries count
/// in Completed) aren't yet rendered by the current TUI but are kept
/// in the event payload so future widget additions don't need a
/// breaking ABI change.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub enum ScrapeEvent {
    /// Emitted up-front for every URL before fetching begins, so the
    /// TUI can render the full URL list immediately.
    Queued { url: String, index: usize },
    /// A fetch attempt is starting (attempt 0 = first try, 1+ = retry).
    RequestStart { url: String, attempt: u32 },
    /// A response came back from the server (transient or final).
    Response {
        url: String,
        status: u16,
        body_size: usize,
        latency_ms: u64,
    },
    /// About to sleep for `backoff_ms` before the next attempt.
    BackoffStart {
        url: String,
        attempt: u32,
        backoff_ms: u64,
    },
    /// Final state — fetch (and optional eval) succeeded.
    Completed {
        url: String,
        final_status: u16,
        title: String,
        total_time_ms: u64,
        retries: u32,
        eval: Option<String>,
    },
    /// Final state — all attempts exhausted, no response.
    Failed {
        url: String,
        error: String,
        attempts: u32,
    },
}

#[inline]
fn emit(tx: Option<&UnboundedSender<ScrapeEvent>>, ev: ScrapeEvent) {
    if let Some(s) = tx {
        let _ = s.send(ev);
    }
}

/// Statuses worth retrying. 5xx are server errors, 429 is the canonical
/// "back off and try again", 408 is request timeout from the server side.
pub fn is_transient_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500..=599)
}

/// Exponential backoff capped at 30s. attempt=0 → base, 1 → 2*base, etc.
pub fn backoff_ms(base_ms: u64, attempt: u32) -> u64 {
    base_ms.saturating_mul(1u64 << attempt.min(7)).min(30_000)
}

#[derive(Serialize)]
pub struct ScrapeRow {
    pub url: String,
    pub title: String,
    pub eval: Option<String>,
    pub time_ms: u64,
    pub worker: usize,
    /// How many retry attempts the worker burned before getting a final
    /// answer (0 means the first request was good).
    pub retries: u32,
    /// HTTP status from the final attempt; 0 if the row never connected.
    pub status: u16,
    /// `--select` results for this URL: text content of every element
    /// matching the selector (or attribute values when `--attr` is set).
    /// Omitted from JSON when `--select` wasn't passed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<Vec<String>>,
}

/// Render the per-URL rows as either tab-separated text or pretty
/// JSON. Pure (no I/O) so the formatting can be unit-tested
/// independently of the network-driven scrape.
pub(crate) fn format_summary(
    rows: &[ScrapeRow],
    format: &str,
    total_time_ms: u64,
    concurrency: usize,
    avg_time_ms: f64,
) -> anyhow::Result<String> {
    use std::fmt::Write as _;
    match format {
        "text" => {
            let mut s = String::new();
            for r in rows {
                writeln!(s, "{}ms\t{}\t{}", r.time_ms, r.url, r.title)?;
            }
            Ok(s)
        }
        _ => {
            #[derive(Serialize)]
            struct Report<'a> {
                total_urls: usize,
                concurrency: usize,
                total_time_ms: u64,
                avg_time_ms: f64,
                results: &'a [ScrapeRow],
            }
            let mut s = serde_json::to_string_pretty(&Report {
                total_urls: rows.len(),
                concurrency,
                total_time_ms,
                avg_time_ms,
                results: rows,
            })?;
            s.push('\n');
            Ok(s)
        }
    }
}

/// Lower-cased host portion of a URL, used as the key for per-host
/// throttling. Returns `None` for URLs that don't parse or that have
/// no host (e.g. a bare path) — those bypass the limiter so they
/// don't all queue against an empty "no-host" bucket.
pub fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .host_str()
        .map(|h| h.to_ascii_lowercase())
}

/// Per-host concurrency limiter for parallel scraping. Lazily creates
/// one `Semaphore` per host so that a `--per-host-concurrency 2` run
/// holds at most 2 in-flight requests against any single origin while
/// still allowing the configured overall `--concurrency` across hosts.
///
/// Cheap to clone (`Arc`-backed). Permits are released when the
/// `OwnedSemaphorePermit` is dropped at the end of each fetch.
#[derive(Clone)]
pub struct HostLimiter {
    limit: usize,
    slots: Arc<StdMutex<HashMap<String, Arc<Semaphore>>>>,
}

impl HostLimiter {
    pub fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            slots: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    /// Acquire one permit for the given host. URLs without a parseable
    /// host bypass the limiter (returns `None` permit) so we don't all
    /// queue them against a single phantom bucket.
    pub async fn acquire(&self, host: Option<&str>) -> Option<OwnedSemaphorePermit> {
        let host = host?;
        let sem = {
            let mut g = self.slots.lock().expect("HostLimiter mutex poisoned");
            g.entry(host.to_ascii_lowercase())
                .or_insert_with(|| Arc::new(Semaphore::new(self.limit)))
                .clone()
        };
        sem.acquire_owned().await.ok()
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn scrape(
    urls: Vec<String>,
    concurrency: usize,
    format: String,
    eval: Option<String>,
    cookie_jar_path: Option<std::path::PathBuf>,
    block_trackers: bool,
    block_hosts: Vec<String>,
    ca_files: Vec<std::path::PathBuf>,
    max_redirects: u32,
    retry: u32,
    retry_delay_ms: u64,
    // Per-host concurrency cap. `None` = no per-host throttle (the
    // overall `--concurrency` is the only ceiling). `Some(n)` caps any
    // single origin to `n` in-flight requests at a time.
    per_host_concurrency: Option<usize>,
    // Override for the outgoing `User-Agent` header. `None` keeps the
    // Fetcher default (`bouncy/<version> (+repo URL)`).
    user_agent: Option<String>,
    // Optional CSS selector for per-row extraction. When set, each row's
    // `selected` field is populated. With `select_attr` also set, the
    // attribute value is extracted instead of text content.
    select: Option<String>,
    select_attr: Option<String>,
    tx: Option<UnboundedSender<ScrapeEvent>>,
) -> anyhow::Result<()> {
    let jar = load_cookie_jar(cookie_jar_path.as_deref())?;
    let fetcher = Arc::new({
        let mut b = bouncy_fetch::Fetcher::builder().max_redirects(max_redirects);
        if let Some(j) = jar.clone() {
            b = b.cookie_jar(j);
        }
        if let Some(bl) = build_blocklist(block_trackers, &block_hosts) {
            b = b.tracker_blocklist(bl);
        }
        for path in &ca_files {
            b = b.ca_file(path);
        }
        if let Some(ua) = user_agent.as_deref() {
            b = b.user_agent(ua);
        }
        b.build()?
    });
    let _ = Fetcher::new;
    let total_start = Instant::now();

    let eval_owned = eval;
    // Build the per-host limiter once and clone the Arc handle into
    // each task. Skipping construction when `per_host_concurrency` is
    // None means no extra map / semaphore allocation in the common case.
    let host_limiter = per_host_concurrency.map(HostLimiter::new);

    // Pre-emit Queued for every URL so the TUI sees the full list
    // before any fetch begins. Indices match the worker-id assignment.
    for (i, url) in urls.iter().enumerate() {
        emit(
            tx.as_ref(),
            ScrapeEvent::Queued {
                url: url.clone(),
                index: i,
            },
        );
    }

    let mut rows: Vec<ScrapeRow> = stream::iter(urls.into_iter().enumerate())
        .map(|(i, url)| {
            let fetcher = fetcher.clone();
            let eval = eval_owned.clone();
            let tx = tx.clone();
            let host_limiter = host_limiter.clone();
            let select = select.clone();
            let select_attr = select_attr.clone();
            async move {
                // Per-host throttle: hold this permit for the lifetime
                // of the per-URL pipeline (all retry attempts plus eval),
                // so retries against the same host count toward the cap.
                let _host_permit = match host_limiter.as_ref() {
                    Some(l) => l.acquire(host_of(&url).as_deref()).await,
                    None => None,
                };
                let start = Instant::now();
                let mut retries = 0u32;
                let mut last_err: Option<String> = None;
                let final_resp = loop {
                    emit(
                        tx.as_ref(),
                        ScrapeEvent::RequestStart {
                            url: url.clone(),
                            attempt: retries,
                        },
                    );
                    let attempt_start = Instant::now();
                    match fetcher.get(&url).await {
                        Ok(r) => {
                            emit(
                                tx.as_ref(),
                                ScrapeEvent::Response {
                                    url: url.clone(),
                                    status: r.status,
                                    body_size: r.body.len(),
                                    latency_ms: attempt_start.elapsed().as_millis() as u64,
                                },
                            );
                            if !is_transient_status(r.status) || retries >= retry {
                                break Ok(r);
                            }
                            // transient + retries left → fall through to backoff
                        }
                        Err(e) => {
                            last_err = Some(e.to_string());
                            if retries >= retry {
                                break Err(e);
                            }
                            // err + retries left → fall through to backoff
                        }
                    }
                    let backoff = backoff_ms(retry_delay_ms, retries);
                    emit(
                        tx.as_ref(),
                        ScrapeEvent::BackoffStart {
                            url: url.clone(),
                            attempt: retries,
                            backoff_ms: backoff,
                        },
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                    retries += 1;
                };
                let time_ms = start.elapsed().as_millis() as u64;
                let (status, title, eval_out, selected) = match final_resp {
                    Ok(r) => {
                        let status = r.status;
                        let title = extract_title(&r.body).ok().flatten().unwrap_or_default();
                        let eval_out = if let Some(expr) = eval.as_deref() {
                            // Per-row V8 boot only when --eval is given.
                            let mut rt =
                                Runtime::new(tokio::runtime::Handle::current(), fetcher.clone());
                            if let Ok(html) = std::str::from_utf8(&r.body) {
                                rt.load(html, &url).ok();
                                rt.run_inline_scripts().ok();
                                rt.eval(expr).ok()
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        // CSS-selector extraction: pure (no V8 boot),
                        // re-parses the same bytes via html5ever.
                        let selected = if let Some(sel) = select.as_deref() {
                            std::str::from_utf8(&r.body).ok().and_then(|html| {
                                let result = match select_attr.as_deref() {
                                    Some(attr) => crate::select::select_attr(html, sel, attr),
                                    None => crate::select::select_text(html, sel),
                                };
                                result.ok()
                            })
                        } else {
                            None
                        };
                        emit(
                            tx.as_ref(),
                            ScrapeEvent::Completed {
                                url: url.clone(),
                                final_status: status,
                                title: title.clone(),
                                total_time_ms: time_ms,
                                retries,
                                eval: eval_out.clone(),
                            },
                        );
                        (status, title, eval_out, selected)
                    }
                    Err(_) => {
                        emit(
                            tx.as_ref(),
                            ScrapeEvent::Failed {
                                url: url.clone(),
                                error: last_err.unwrap_or_else(|| "fetch failed".to_string()),
                                attempts: retries.saturating_add(1),
                            },
                        );
                        (0, String::new(), None, None)
                    }
                };
                ScrapeRow {
                    url,
                    title,
                    eval: eval_out,
                    time_ms,
                    worker: i % concurrency.max(1),
                    retries,
                    status,
                    selected,
                }
            }
        })
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;

    rows.sort_by(|a, b| a.worker.cmp(&b.worker).then_with(|| a.url.cmp(&b.url)));
    let total_time_ms = total_start.elapsed().as_millis() as u64;
    let avg_time_ms = if !rows.is_empty() {
        rows.iter().map(|r| r.time_ms).sum::<u64>() as f64 / rows.len() as f64
    } else {
        0.0
    };

    // When `tx` is set, the caller is rendering a live TUI on the
    // alt-screen — writing JSON / text to stdout from this task races
    // the TUI's draws and corrupts the display. Skip the dump in that
    // mode; the TUI is the user-facing output.
    if tx.is_none() {
        let summary = format_summary(&rows, &format, total_time_ms, concurrency, avg_time_ms)?;
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        out.write_all(summary.as_bytes())?;
        drop(out);
    }

    if let Some(j) = jar {
        save_cookie_jar(cookie_jar_path.as_deref(), &j)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_transient_handles_canonical_codes() {
        assert!(is_transient_status(408));
        assert!(is_transient_status(429));
        assert!(is_transient_status(500));
        assert!(is_transient_status(502));
        assert!(is_transient_status(503));
        assert!(is_transient_status(504));
        assert!(is_transient_status(599));
    }

    #[test]
    fn is_transient_rejects_non_transient_codes() {
        assert!(!is_transient_status(200));
        assert!(!is_transient_status(201));
        assert!(!is_transient_status(301));
        assert!(!is_transient_status(400));
        assert!(!is_transient_status(404));
        assert!(!is_transient_status(407));
    }

    #[test]
    fn backoff_grows_exponentially() {
        assert_eq!(backoff_ms(250, 0), 250);
        assert_eq!(backoff_ms(250, 1), 500);
        assert_eq!(backoff_ms(250, 2), 1000);
        assert_eq!(backoff_ms(250, 3), 2000);
        assert_eq!(backoff_ms(250, 4), 4000);
    }

    #[test]
    fn backoff_caps_at_30s() {
        // 250 * 2^7 = 32000, capped at 30000
        assert_eq!(backoff_ms(250, 7), 30_000);
        assert_eq!(backoff_ms(250, 100), 30_000);
        assert_eq!(backoff_ms(1_000_000, 0), 30_000);
    }

    #[test]
    fn backoff_handles_zero_base() {
        assert_eq!(backoff_ms(0, 0), 0);
        assert_eq!(backoff_ms(0, 5), 0);
    }

    fn row(url: &str, title: &str, time_ms: u64, status: u16) -> ScrapeRow {
        ScrapeRow {
            url: url.into(),
            title: title.into(),
            eval: None,
            time_ms,
            worker: 0,
            retries: 0,
            status,
            selected: None,
        }
    }

    #[test]
    fn format_text_summary_emits_one_line_per_row() {
        let rows = vec![
            row("https://a", "Alpha", 142, 200),
            row("https://b", "Bravo", 311, 200),
        ];
        let s = format_summary(&rows, "text", 500, 4, 226.5).unwrap();
        assert_eq!(s, "142ms\thttps://a\tAlpha\n311ms\thttps://b\tBravo\n");
    }

    #[test]
    fn format_json_summary_includes_report_envelope() {
        let rows = vec![row("https://a", "T", 100, 200)];
        let s = format_summary(&rows, "json", 200, 8, 100.0).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["total_urls"], 1);
        assert_eq!(v["concurrency"], 8);
        assert_eq!(v["total_time_ms"], 200);
        assert_eq!(v["avg_time_ms"], 100.0);
        assert_eq!(v["results"][0]["url"], "https://a");
        assert_eq!(v["results"][0]["status"], 200);
        assert!(s.ends_with('\n'));
    }

    #[test]
    fn format_summary_empty_rows() {
        assert_eq!(format_summary(&[], "text", 0, 1, 0.0).unwrap(), "");
        let v: serde_json::Value =
            serde_json::from_str(&format_summary(&[], "json", 0, 1, 0.0).unwrap()).unwrap();
        assert_eq!(v["total_urls"], 0);
        assert_eq!(v["results"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn format_summary_unknown_format_falls_back_to_json() {
        // Mirrors the dispatch in scrape() — anything not "text" is JSON.
        let rows = vec![row("https://a", "T", 1, 200)];
        let s = format_summary(&rows, "yaml", 1, 1, 1.0).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&s).is_ok());
    }

    #[test]
    fn host_of_extracts_hostname() {
        assert_eq!(
            host_of("https://example.com/path"),
            Some("example.com".into())
        );
        assert_eq!(host_of("http://Example.COM/"), Some("example.com".into()));
        assert_eq!(
            host_of("https://sub.example.com:8443/x"),
            Some("sub.example.com".into())
        );
    }

    #[test]
    fn host_of_handles_ipv6_bracketed() {
        // url::Url returns the host without brackets for IPv6.
        assert_eq!(host_of("http://[::1]:8080/"), Some("[::1]".into()));
    }

    #[test]
    fn host_of_returns_none_for_non_url() {
        assert_eq!(host_of("/relative/path"), None);
        assert_eq!(host_of("not-a-url"), None);
        assert_eq!(host_of(""), None);
    }

    #[tokio::test]
    async fn host_limiter_blocks_past_per_host_limit() {
        // With limit=2, the third concurrent acquire on the same host
        // must wait until one of the first two permits is dropped.
        let limiter = HostLimiter::new(2);
        let p1 = limiter.acquire(Some("example.com")).await.unwrap();
        let p2 = limiter.acquire(Some("example.com")).await.unwrap();
        // Third acquire should not complete within 50ms while p1+p2 are held.
        let third = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            limiter.acquire(Some("example.com")),
        )
        .await;
        assert!(
            third.is_err(),
            "third acquire should have timed out, got {:?}",
            third
        );
        // Drop one permit; the next acquire should succeed quickly.
        drop(p1);
        let p3 = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            limiter.acquire(Some("example.com")),
        )
        .await
        .expect("third acquire after drop should not time out")
        .unwrap();
        drop(p2);
        drop(p3);
    }

    #[tokio::test]
    async fn host_limiter_does_not_block_different_hosts() {
        // limit=1 per host, but two different hosts must both proceed.
        let limiter = HostLimiter::new(1);
        let p_a = limiter.acquire(Some("a.example")).await.unwrap();
        let p_b = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            limiter.acquire(Some("b.example")),
        )
        .await
        .expect("different host should not block")
        .unwrap();
        drop(p_a);
        drop(p_b);
    }

    #[tokio::test]
    async fn host_limiter_bypasses_when_host_is_none() {
        // URLs we couldn't parse a host out of (e.g. a relative path)
        // skip the limiter entirely; they get None back.
        let limiter = HostLimiter::new(1);
        assert!(limiter.acquire(None).await.is_none());
        assert!(limiter.acquire(None).await.is_none());
    }

    #[test]
    fn host_limiter_treats_zero_limit_as_one() {
        // A 0-limit semaphore would deadlock immediately; coerce to 1.
        let limiter = HostLimiter::new(0);
        assert_eq!(limiter.limit, 1);
    }
}
