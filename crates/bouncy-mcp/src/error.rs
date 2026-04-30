use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("fetch: {0}")]
    Fetch(#[from] bouncy_fetch::Error),
    #[error("extract: {0}")]
    Extract(#[from] bouncy_extract::Error),
    #[error("js: {0}")]
    Js(#[from] bouncy_js::Error),
    #[error("bad input: {0}")]
    BadInput(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("url: {0}")]
    Url(#[from] url::ParseError),
    #[error("utf8: {0}")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("anyhow: {0}")]
    Anyhow(#[from] anyhow::Error),
    #[error("internal: {0}")]
    Internal(String),
}

impl From<ToolError> for rmcp::ErrorData {
    fn from(e: ToolError) -> Self {
        rmcp::ErrorData::internal_error(e.to_string(), None)
    }
}
