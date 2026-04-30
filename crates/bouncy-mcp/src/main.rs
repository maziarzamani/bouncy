use anyhow::Result;
use bouncy_mcp::BoinkServer;
use rmcp::{transport::stdio, ServiceExt};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Tracing must NEVER write to stdout — JSON-RPC framing lives there.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("bouncy-mcp starting (stdio transport)");

    let server = BoinkServer::new()?;
    let service = server.serve(stdio()).await.inspect_err(|e| {
        tracing::error!("serve error: {e}");
    })?;
    service.waiting().await?;
    Ok(())
}
