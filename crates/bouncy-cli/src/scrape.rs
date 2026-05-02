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

use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

use bouncy_extract::extract_title;
use bouncy_fetch::Fetcher;
use bouncy_js::Runtime;
use futures_util::stream::{self, StreamExt};
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

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
}

#[derive(Serialize)]
pub struct ScrapeReport {
    pub total_urls: usize,
    pub concurrency: usize,
    pub total_time_ms: u64,
    pub avg_time_ms: f64,
    pub results: Vec<ScrapeRow>,
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
        b.build()?
    });
    let _ = Fetcher::new;
    let total_start = Instant::now();

    let eval_owned = eval;

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
            async move {
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
                let (status, title, eval_out) = match final_resp {
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
                        (status, title, eval_out)
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
                        (0, String::new(), None)
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

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match format.as_str() {
        "text" => {
            for r in &rows {
                writeln!(out, "{}ms\t{}\t{}", r.time_ms, r.url, r.title)?;
            }
        }
        _ => {
            let report = ScrapeReport {
                total_urls: rows.len(),
                concurrency,
                total_time_ms,
                avg_time_ms,
                results: rows,
            };
            writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
        }
    }
    drop(out);

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
}
