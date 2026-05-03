use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use bouncy_fetch::{FetchRequest, Fetcher, Response};
use bouncy_js::Runtime;

use crate::error::ToolError;
use crate::tools::Cookie;

const MAX_NAV_HOPS: u32 = 10;

#[allow(clippy::too_many_arguments)]
pub fn build_request(
    url: &str,
    method: Option<&str>,
    headers: Option<&HashMap<String, String>>,
    body: Option<&str>,
    cookies: Option<&[Cookie]>,
    basic_auth: Option<(&str, &str)>,
    user_agent: Option<&str>,
) -> FetchRequest {
    let mut req = FetchRequest::new(url);
    if let Some(m) = method {
        req = req.method(m);
    }
    if let Some(h) = headers {
        for (k, v) in h {
            req = req.header(k, v);
        }
    }
    if let Some(b) = body {
        req = req.body_str(b.to_string());
    }
    if let Some(cs) = cookies {
        if !cs.is_empty() {
            let joined = cs
                .iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join("; ");
            req = req.header("Cookie", joined);
        }
    }
    if let Some((user, pass)) = basic_auth {
        let encoded = B64.encode(format!("{user}:{pass}"));
        req = req.header("Authorization", format!("Basic {encoded}"));
    }
    if let Some(ua) = user_agent {
        // Per-request UA overrides the shared Fetcher's default via the
        // last-write-wins logic in bouncy-fetch.
        req = req.header("User-Agent", ua);
    }
    req
}

/// Run a CSS selector against an HTML body and return one entry per
/// match. When `attr` is set, returns attribute values (skipping
/// elements without that attribute); otherwise returns text content.
/// Used by both `do_fetch` and `do_scrape` for the `select` field.
pub fn select_from_html(
    html: &str,
    selector: &str,
    attr: Option<&str>,
) -> Result<Vec<String>, ToolError> {
    let doc = bouncy_dom::Document::parse(html)
        .map_err(|e| ToolError::Internal(format!("html parse: {e}")))?;
    let ids = doc.query_selector_all(selector);
    Ok(match attr {
        Some(a) => ids
            .into_iter()
            .filter_map(|id| doc.get_attribute(id, a))
            .collect(),
        None => ids.into_iter().map(|id| doc.text_content(id)).collect(),
    })
}

pub fn looks_textual(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            let s = s.to_ascii_lowercase();
            s.starts_with("text/")
                || s.contains("json")
                || s.contains("xml")
                || s.contains("javascript")
                || s.contains("html")
        })
        .unwrap_or(false)
}

pub fn body_to_strings(resp: &Response, max_bytes: u64) -> (Option<String>, Option<String>, bool) {
    let truncated = (resp.body.len() as u64) > max_bytes;
    let slice: &[u8] = if truncated {
        &resp.body[..max_bytes as usize]
    } else {
        &resp.body[..]
    };
    if looks_textual(&resp.headers) {
        match std::str::from_utf8(slice) {
            Ok(s) => (Some(s.to_string()), None, truncated),
            Err(_) => (None, Some(B64.encode(slice)), truncated),
        }
    } else {
        (None, Some(B64.encode(slice)), truncated)
    }
}

pub fn headers_to_map(h: &http::HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::with_capacity(h.len());
    for (name, value) in h.iter() {
        if let Ok(v) = value.to_str() {
            out.insert(name.as_str().to_string(), v.to_string());
        }
    }
    out
}

/// Run an async fetch with a per-call timeout. Wraps the request in
/// `tokio::time::timeout` and converts the elapsed case into a synthetic
/// `bouncy_fetch::Error::Timeout` so the MCP error path is uniform.
pub async fn fetch_with_timeout(
    fetcher: &Fetcher,
    req: FetchRequest,
    timeout: Duration,
) -> Result<Response, ToolError> {
    match tokio::time::timeout(timeout, fetcher.request(req)).await {
        Ok(r) => Ok(r?),
        Err(_) => Err(ToolError::Fetch(bouncy_fetch::Error::Timeout(timeout))),
    }
}

pub struct JsRender<'a> {
    pub handle: tokio::runtime::Handle,
    pub fetcher: Arc<Fetcher>,
    pub rt: &'a mut Runtime,
    pub initial_html: &'a str,
    pub initial_url: &'a str,
    pub selector: Option<&'a str>,
    pub selector_timeout_ms: u64,
    pub eval_expr: Option<&'a str>,
}

/// Render a fetched HTML page through V8: load → run inline scripts →
/// follow up to MAX_NAV_HOPS `location.href` navigations → optional
/// selector wait → optional eval. Mirrors the JS branch of `bouncy fetch`
/// in [crates/bouncy-cli/src/main.rs].
///
/// Sync because `bouncy_js::Runtime` holds a `v8::OwnedIsolate` (`!Send`)
/// across the awaits in `wait_for_selector`. Caller wraps this in
/// `tokio::task::spawn_blocking` so the resulting future is `Send`.
/// Internal awaits (fetch + selector poll) drive on the supplied
/// `tokio::runtime::Handle` via `block_on`.
pub fn render_js_blocking(
    args: JsRender<'_>,
) -> Result<(Option<String>, String, String), ToolError> {
    let JsRender {
        handle,
        fetcher,
        rt,
        initial_html,
        initial_url,
        selector,
        selector_timeout_ms,
        eval_expr,
    } = args;
    rt.load(initial_html, initial_url)?;
    rt.run_inline_scripts()?;
    let mut current_url = initial_url.to_string();
    let mut hops = 0u32;
    while let Some(next_url) = rt.take_pending_nav() {
        if hops >= MAX_NAV_HOPS {
            break;
        }
        hops += 1;
        let next_resp = handle.block_on(fetcher.request(FetchRequest::new(&next_url)))?;
        let next_html = std::str::from_utf8(&next_resp.body)?;
        rt.load(next_html, &next_url)?;
        rt.run_inline_scripts()?;
        current_url = next_url;
    }
    if let Some(sel) = selector {
        let _ = handle.block_on(rt.wait_for_selector(sel, selector_timeout_ms))?;
    }
    let eval_result = match eval_expr {
        Some(expr) => Some(rt.eval(expr)?),
        None => None,
    };
    let html = rt.dump_html()?;
    Ok((eval_result, html, current_url))
}

/// Sleep for an exponentially-increasing backoff, capped at 30s. Mirrors
/// the constants in `bouncy scrape`'s retry loop.
pub async fn backoff_sleep(initial_ms: u64, attempt: u32) {
    let ms = backoff_ms(initial_ms, attempt);
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// Pure helper extracted so the math is unit-testable without sleeping.
pub fn backoff_ms(initial_ms: u64, attempt: u32) -> u64 {
    initial_ms
        .saturating_mul(1u64 << attempt.min(20))
        .min(30_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http::header::CONTENT_TYPE;
    use http::HeaderMap;

    fn make_response(body: &[u8], content_type: Option<&str>) -> Response {
        let mut headers = HeaderMap::new();
        if let Some(ct) = content_type {
            headers.insert(CONTENT_TYPE, ct.parse().unwrap());
        }
        Response {
            status: 200,
            body: Bytes::copy_from_slice(body),
            headers,
        }
    }

    #[test]
    fn looks_textual_handles_text_html() {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, "text/html; charset=utf-8".parse().unwrap());
        assert!(looks_textual(&h));
    }

    #[test]
    fn looks_textual_handles_application_json() {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, "application/json".parse().unwrap());
        assert!(looks_textual(&h));
    }

    #[test]
    fn looks_textual_handles_xml_and_javascript() {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, "application/xml".parse().unwrap());
        assert!(looks_textual(&h));
        h.insert(CONTENT_TYPE, "application/javascript".parse().unwrap());
        assert!(looks_textual(&h));
    }

    #[test]
    fn looks_textual_rejects_image_and_octet_stream() {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, "image/png".parse().unwrap());
        assert!(!looks_textual(&h));
        h.insert(CONTENT_TYPE, "application/octet-stream".parse().unwrap());
        assert!(!looks_textual(&h));
    }

    #[test]
    fn looks_textual_returns_false_when_header_absent() {
        assert!(!looks_textual(&HeaderMap::new()));
    }

    #[test]
    fn body_to_strings_returns_text_for_text_body() {
        let resp = make_response(b"<html>hi</html>", Some("text/html"));
        let (text, b64, truncated) = body_to_strings(&resp, 1024);
        assert_eq!(text.as_deref(), Some("<html>hi</html>"));
        assert!(b64.is_none());
        assert!(!truncated);
    }

    #[test]
    fn body_to_strings_returns_base64_for_binary_body() {
        let resp = make_response(&[0x89, 0x50, 0x4E, 0x47], Some("image/png"));
        let (text, b64, truncated) = body_to_strings(&resp, 1024);
        assert!(text.is_none());
        assert_eq!(b64.as_deref(), Some("iVBORw=="));
        assert!(!truncated);
    }

    #[test]
    fn body_to_strings_falls_back_to_base64_on_invalid_utf8_in_text_body() {
        let resp = make_response(&[0xff, 0xfe, 0xfd], Some("text/plain"));
        let (text, b64, _) = body_to_strings(&resp, 1024);
        assert!(text.is_none());
        assert!(b64.is_some());
    }

    #[test]
    fn body_to_strings_truncates_oversize_text() {
        let body = "x".repeat(2000);
        let resp = make_response(body.as_bytes(), Some("text/plain"));
        let (text, b64, truncated) = body_to_strings(&resp, 100);
        assert!(truncated);
        assert_eq!(text.as_deref().map(str::len), Some(100));
        assert!(b64.is_none());
    }

    #[test]
    fn body_to_strings_falls_back_to_base64_for_missing_content_type() {
        let resp = make_response(b"hello", None);
        let (text, b64, _) = body_to_strings(&resp, 1024);
        // No content-type → not textual → base64.
        assert!(text.is_none());
        assert!(b64.is_some());
    }

    #[test]
    fn headers_to_map_round_trips_simple_headers() {
        let mut h = HeaderMap::new();
        h.insert("x-foo", "bar".parse().unwrap());
        h.insert(CONTENT_TYPE, "text/plain".parse().unwrap());
        let map = headers_to_map(&h);
        assert_eq!(map.get("x-foo"), Some(&"bar".to_string()));
        assert_eq!(map.get("content-type"), Some(&"text/plain".to_string()));
    }

    #[test]
    fn build_request_sets_method_and_headers() {
        let mut headers = HashMap::new();
        headers.insert("X-Custom".to_string(), "v".to_string());
        let req = build_request(
            "https://example.com/p",
            Some("POST"),
            Some(&headers),
            Some(r#"{"a":1}"#),
            None,
            None,
            None,
        );
        assert_eq!(req.url, "https://example.com/p");
        assert_eq!(req.method, "POST");
        assert!(req.headers.iter().any(|(k, v)| k == "X-Custom" && v == "v"));
        assert_eq!(req.body, Bytes::from_static(b"{\"a\":1}"));
    }

    #[test]
    fn build_request_joins_cookies_with_semicolon() {
        let cookies = vec![
            Cookie {
                name: "a".into(),
                value: "1".into(),
            },
            Cookie {
                name: "b".into(),
                value: "two".into(),
            },
        ];
        let req = build_request(
            "https://example.com",
            None,
            None,
            None,
            Some(&cookies),
            None,
            None,
        );
        let cookie = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Cookie"))
            .map(|(_, v)| v.as_str());
        assert_eq!(cookie, Some("a=1; b=two"));
    }

    #[test]
    fn build_request_omits_cookie_header_when_list_empty() {
        let req = build_request(
            "https://example.com",
            None,
            None,
            None,
            Some(&[]),
            None,
            None,
        );
        assert!(req
            .headers
            .iter()
            .all(|(k, _)| !k.eq_ignore_ascii_case("Cookie")));
    }

    #[test]
    fn build_request_encodes_basic_auth() {
        // base64("user:pw") = "dXNlcjpwdw=="
        let req = build_request(
            "https://example.com",
            None,
            None,
            None,
            None,
            Some(("user", "pw")),
            None,
        );
        let auth = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Authorization"))
            .map(|(_, v)| v.as_str());
        assert_eq!(auth, Some("Basic dXNlcjpwdw=="));
    }

    #[test]
    fn backoff_ms_grows_exponentially_then_caps() {
        assert_eq!(backoff_ms(250, 0), 250);
        assert_eq!(backoff_ms(250, 1), 500);
        assert_eq!(backoff_ms(250, 2), 1000);
        assert_eq!(backoff_ms(250, 3), 2000);
        // Cap is 30_000 ms.
        assert_eq!(backoff_ms(250, 20), 30_000);
        assert_eq!(backoff_ms(250, 100), 30_000);
    }

    #[test]
    fn backoff_ms_handles_zero_initial() {
        assert_eq!(backoff_ms(0, 0), 0);
        assert_eq!(backoff_ms(0, 5), 0);
    }
}
