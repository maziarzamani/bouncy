use std::sync::Arc;
use std::time::{Duration, Instant};

use bouncy_fetch::Fetcher;
use bouncy_js::Runtime;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use url::Url;

use crate::error::ToolError;
use crate::glue;
use crate::tools::*;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_MAX_BODY_BYTES: u64 = 1_048_576; // 1 MB
const DEFAULT_RETRY_INITIAL_MS: u64 = 250;

#[derive(Clone)]
pub struct BouncyServer {
    fetcher: Arc<Fetcher>,
    // The `#[tool_handler]` macro on `impl ServerHandler` reads this
    // through generated code that escapes dead-code analysis.
    #[allow(dead_code)]
    tool_router: ToolRouter<BouncyServer>,
}

#[tool_router]
impl BouncyServer {
    pub fn new() -> anyhow::Result<Self> {
        let fetcher = Arc::new(Fetcher::new()?);
        Ok(Self {
            fetcher,
            tool_router: Self::tool_router(),
        })
    }

    fn ok<T: serde::Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
        let json = serde_json::to_string_pretty(value)
            .map_err(|e| ErrorData::internal_error(format!("serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    async fn do_fetch(&self, input: FetchInput) -> Result<FetchOutput, ToolError> {
        let timeout = Duration::from_millis(input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        let max_bytes = input.max_body_bytes.unwrap_or(DEFAULT_MAX_BODY_BYTES);
        let req = glue::build_request(
            &input.url,
            input.method.as_deref(),
            input.headers.as_ref(),
            input.body.as_deref(),
            input.cookies.as_deref(),
            input
                .basic_auth
                .as_ref()
                .map(|a| (a.user.as_str(), a.pass.as_str())),
            input.user_agent.as_deref(),
        );
        let resp = glue::fetch_with_timeout(&self.fetcher, req, timeout).await?;
        // Run --select against the body before truncation. We want the
        // selector to see the full document even if `max_body_bytes`
        // would clip the returned body_text.
        let selected = if let Some(sel) = input.select.as_deref() {
            std::str::from_utf8(&resp.body)
                .ok()
                .map(|html| glue::select_from_html(html, sel, input.select_attr.as_deref()))
                .transpose()?
        } else {
            None
        };
        let (text, b64, truncated) = glue::body_to_strings(&resp, max_bytes);
        Ok(FetchOutput {
            status: resp.status,
            headers: glue::headers_to_map(&resp.headers),
            body_text: text,
            body_base64: b64,
            truncated,
            final_url: input.url,
            selected,
        })
    }

    async fn do_js_eval(&self, input: JsEvalInput) -> Result<JsEvalOutput, ToolError> {
        let timeout = Duration::from_millis(input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        let req = glue::build_request(
            &input.url,
            None,
            None,
            None,
            input.cookies.as_deref(),
            None,
            None,
        );
        let resp = glue::fetch_with_timeout(&self.fetcher, req, timeout).await?;
        let html_str = std::str::from_utf8(&resp.body)?.to_string();
        let fetcher = self.fetcher.clone();
        let url = input.url.clone();
        let wait_for = input.wait_for.clone();
        let eval_expr = input.eval.clone();
        let timeout_ms = input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        // V8 work runs on a blocking-pool thread because Runtime is !Send.
        // The handler future stays Send (just an awaited JoinHandle).
        let (eval_result, html, final_url) = tokio::task::spawn_blocking(
            move || -> Result<(Option<String>, String, String), ToolError> {
                let handle = tokio::runtime::Handle::current();
                let mut rt = Runtime::new(handle.clone(), fetcher.clone());
                glue::render_js_blocking(glue::JsRender {
                    handle,
                    fetcher,
                    rt: &mut rt,
                    initial_html: &html_str,
                    initial_url: &url,
                    selector: wait_for.as_deref(),
                    selector_timeout_ms: timeout_ms,
                    eval_expr: Some(&eval_expr),
                })
            },
        )
        .await
        .map_err(|e| ToolError::Internal(format!("join: {e}")))??;
        Ok(JsEvalOutput {
            result: eval_result.unwrap_or_default(),
            html,
            final_url,
        })
    }

    async fn do_scrape(&self, input: ScrapeInput) -> Result<ScrapeOutput, ToolError> {
        let started = Instant::now();
        let timeout = Duration::from_millis(input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        let max_retries = input.max_retries.unwrap_or(0);
        let needs_js = input.eval.is_some() || input.selector.is_some();
        let mut last_err: Option<ToolError> = None;
        for attempt in 0..=max_retries {
            if attempt > 0 {
                glue::backoff_sleep(DEFAULT_RETRY_INITIAL_MS, attempt - 1).await;
            }
            let req = glue::build_request(
                &input.url,
                None,
                input.headers.as_ref(),
                None,
                input.cookies.as_deref(),
                None,
                input.user_agent.as_deref(),
            );
            let resp = match glue::fetch_with_timeout(&self.fetcher, req, timeout).await {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            let transient = resp.status == 429 || resp.status >= 500;
            if transient && attempt < max_retries {
                last_err = Some(ToolError::Internal(format!(
                    "transient HTTP {} on attempt {attempt}",
                    resp.status
                )));
                continue;
            }
            let html_str = std::str::from_utf8(&resp.body)?.to_string();
            let (eval_result, html, final_url) = if needs_js {
                let fetcher = self.fetcher.clone();
                let url = input.url.clone();
                let selector = input.selector.clone();
                let eval_expr = input.eval.clone();
                let timeout_ms = input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
                tokio::task::spawn_blocking(
                    move || -> Result<(Option<String>, String, String), ToolError> {
                        let handle = tokio::runtime::Handle::current();
                        let mut rt = Runtime::new(handle.clone(), fetcher.clone());
                        glue::render_js_blocking(glue::JsRender {
                            handle,
                            fetcher,
                            rt: &mut rt,
                            initial_html: &html_str,
                            initial_url: &url,
                            selector: selector.as_deref(),
                            selector_timeout_ms: timeout_ms,
                            eval_expr: eval_expr.as_deref(),
                        })
                    },
                )
                .await
                .map_err(|e| ToolError::Internal(format!("join: {e}")))??
            } else {
                (None, html_str, input.url.clone())
            };
            let selected = if let Some(sel) = input.select.as_deref() {
                Some(glue::select_from_html(
                    &html,
                    sel,
                    input.select_attr.as_deref(),
                )?)
            } else {
                None
            };
            return Ok(ScrapeOutput {
                url: final_url,
                status: resp.status,
                html,
                eval_result,
                took_ms: started.elapsed().as_millis() as u64,
                selected,
            });
        }
        Err(last_err.unwrap_or_else(|| {
            ToolError::Internal(format!("scrape exhausted {max_retries} retries"))
        }))
    }

    #[tool(
        description = "Fetch a URL over HTTP/HTTPS with no parsing. Returns status, headers, and the raw body (text when text-ish, base64 otherwise)."
    )]
    async fn fetch(
        &self,
        Parameters(input): Parameters<FetchInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let out = self.do_fetch(input).await.map_err(ErrorData::from)?;
        Self::ok(&out)
    }

    #[tool(description = "Extract the <title> from an HTML string.")]
    fn extract_title(
        &self,
        Parameters(input): Parameters<ExtractTitleInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let title = bouncy_extract::extract_title(input.html.as_bytes())
            .map_err(|e| ErrorData::from(ToolError::from(e)))?;
        Self::ok(&ExtractTitleOutput { title })
    }

    #[tool(description = "Extract visible body text from an HTML string.")]
    fn extract_text(
        &self,
        Parameters(input): Parameters<ExtractTextInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let text = bouncy_extract::extract_text(input.html.as_bytes())
            .map_err(|e| ErrorData::from(ToolError::from(e)))?;
        Self::ok(&ExtractTextOutput { text })
    }

    #[tool(description = "Extract <a href> links from HTML, resolved against a base URL.")]
    fn extract_links(
        &self,
        Parameters(input): Parameters<ExtractLinksInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let base = Url::parse(&input.base_url).map_err(|e| ErrorData::from(ToolError::from(e)))?;
        let links = bouncy_extract::extract_links(input.html.as_bytes(), &base)
            .map_err(|e| ErrorData::from(ToolError::from(e)))?;
        let out = ExtractLinksOutput {
            links: links
                .into_iter()
                .map(|l| LinkOut {
                    url: l.url,
                    text: l.text,
                })
                .collect(),
        };
        Self::ok(&out)
    }

    #[tool(
        description = "Fetch a URL, boot V8, run a JavaScript expression, and return its result. Use for pages that need scripts to render."
    )]
    async fn js_eval(
        &self,
        Parameters(input): Parameters<JsEvalInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let out = self.do_js_eval(input).await.map_err(ErrorData::from)?;
        Self::ok(&out)
    }

    #[tool(
        description = "High-level scrape of a single URL: auto JS-vs-static branch, optional eval/selector wait, configurable retries on transient errors."
    )]
    async fn scrape(
        &self,
        Parameters(input): Parameters<ScrapeInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let out = self.do_scrape(input).await.map_err(ErrorData::from)?;
        Self::ok(&out)
    }

    #[tool(
        description = "Scrape multiple URLs, one at a time on this single-threaded server. (max_concurrency is currently advisory; tool calls are serialized to keep V8 isolates thread-safe.)"
    )]
    async fn scrape_many(
        &self,
        Parameters(input): Parameters<ScrapeManyInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // Note: per_host_concurrency is accepted on the input for parity
        // with the CLI, but the MCP server currently runs scrapes
        // sequentially (one V8 isolate at a time), so it has no
        // operational effect here today. Documented in the tool
        // description so callers aren't surprised.
        let _ = input.per_host_concurrency;
        let mut results = Vec::with_capacity(input.urls.len());
        for url in &input.urls {
            let single = ScrapeInput {
                url: url.clone(),
                eval: input.eval.clone(),
                selector: input.selector.clone(),
                headers: None,
                timeout_ms: input.timeout_ms,
                max_retries: Some(0),
                cookies: None,
                user_agent: input.user_agent.clone(),
                select: input.select.clone(),
                select_attr: input.select_attr.clone(),
            };
            match self.do_scrape(single).await {
                Ok(o) => results.push(ScrapeManyResult {
                    url: o.url,
                    ok: true,
                    status: Some(o.status),
                    html: Some(o.html),
                    eval_result: o.eval_result,
                    error: None,
                    selected: o.selected,
                }),
                Err(e) => results.push(ScrapeManyResult {
                    url: url.clone(),
                    ok: false,
                    status: None,
                    html: None,
                    eval_result: None,
                    error: Some(e.to_string()),
                    selected: None,
                }),
            }
        }
        Self::ok(&ScrapeManyOutput { results })
    }
}

#[tool_handler]
impl ServerHandler for BouncyServer {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo is #[non_exhaustive] so we mutate Default rather than
        // use a struct literal.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "bouncy — fast headless scraping. Tools: fetch (raw HTTP), \
             extract_title / extract_text / extract_links (static HTML), \
             js_eval (V8), scrape (one URL, auto JS-vs-static + retries), \
             scrape_many (URL list, sequential)."
                .into(),
        );
        info
    }
}
