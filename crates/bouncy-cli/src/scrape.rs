//! `bouncy scrape` — parallel URL scraper.
//!
//! Per-URL pipeline: fetch (with retries + exponential backoff on
//! transient failures) → optional V8 eval → row collected. Results are
//! buffered then emitted as JSON or tab-separated text.

use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

use bouncy_extract::extract_title;
use bouncy_fetch::Fetcher;
use bouncy_js::Runtime;
use futures_util::stream::{self, StreamExt};
use serde::Serialize;

use crate::{build_blocklist, load_cookie_jar, save_cookie_jar};

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
    format: &str,
    eval: Option<&str>,
    cookie_jar_path: Option<&std::path::Path>,
    block_trackers: bool,
    block_hosts: &[String],
    ca_files: &[std::path::PathBuf],
    max_redirects: u32,
    retry: u32,
    retry_delay_ms: u64,
) -> anyhow::Result<()> {
    let jar = load_cookie_jar(cookie_jar_path)?;
    let fetcher = Arc::new({
        let mut b = bouncy_fetch::Fetcher::builder().max_redirects(max_redirects);
        if let Some(j) = jar.clone() {
            b = b.cookie_jar(j);
        }
        if let Some(bl) = build_blocklist(block_trackers, block_hosts) {
            b = b.tracker_blocklist(bl);
        }
        for path in ca_files {
            b = b.ca_file(path);
        }
        b.build()?
    });
    let _ = Fetcher::new;
    let total_start = Instant::now();

    let eval_owned = eval.map(|s| s.to_string());

    let mut rows: Vec<ScrapeRow> = stream::iter(urls.into_iter().enumerate())
        .map(|(i, url)| {
            let fetcher = fetcher.clone();
            let eval = eval_owned.clone();
            async move {
                let start = Instant::now();
                let mut retries = 0u32;
                let final_resp = loop {
                    match fetcher.get(&url).await {
                        Ok(r) if !is_transient_status(r.status) => break Ok(r),
                        Ok(r) if retries >= retry => break Ok(r),
                        Err(e) if retries >= retry => break Err(e),
                        // Either a transient HTTP status with retries left,
                        // or a network error with retries left — back off
                        // and try again.
                        Ok(_) | Err(_) => {
                            let backoff = backoff_ms(retry_delay_ms, retries);
                            tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                            retries += 1;
                        }
                    }
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
                        (status, title, eval_out)
                    }
                    Err(_) => (0, String::new(), None),
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
    match format {
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
        save_cookie_jar(cookie_jar_path, &j)?;
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
