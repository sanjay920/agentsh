//! Entry point for the agentsh MCP server.
//!
//! Initializes tracing (to stderr, so it doesn't interfere with MCP stdio transport),
//! creates the server, and serves on stdin/stdout.

use agentsh::server::AgentshServer;
use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::{self, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing to stderr (stdout is used for MCP JSON-RPC).
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("Starting agentsh MCP server v{}", env!("CARGO_PKG_VERSION"));

    let server = AgentshServer::new();
    let service = server.serve(stdio()).await.inspect_err(|e| {
        tracing::error!("serving error: {:?}", e);
    })?;

    service.waiting().await?;
    tracing::info!("agentsh server shut down");
    Ok(())
}
