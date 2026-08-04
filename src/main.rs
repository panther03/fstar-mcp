//! F* MCP Server - stdio front-end for F*'s IDE protocol.

use fstar_mcp::mcp::{create_fstar_server, DiscoveryAwareStdioTransport, SESSION_MANAGER};
use fstar_mcp::session::DEFAULT_SWEEP_PERIOD_SECS;
use fstar_mcp::set_verbose;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let verbose = args.iter().any(|a| a == "--verbose" || a == "-v");
    set_verbose(verbose);

    // Get sweep period from environment or use default
    let sweep_period: u64 = std::env::var("FSTAR_MCP_SWEEP_PERIOD")
        .unwrap_or_else(|_| DEFAULT_SWEEP_PERIOD_SECS.to_string())
        .parse()
        .unwrap_or(DEFAULT_SWEEP_PERIOD_SECS);

    // Initialize logging - use debug level if verbose
    let default_filter = if verbose {
        "fstar_mcp=debug,pmcp=debug"
    } else {
        "fstar_mcp=info,pmcp=info"
    };

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.into()),
        )
        .init();

    let server = create_fstar_server()?;
    let sweeper_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(sweep_period));
        loop {
            interval.tick().await;
            let count = SESSION_MANAGER.sweep_marked_sessions().await;
            if count > 0 {
                tracing::info!(count, "Swept marked sessions");
            }
        }
    });

    info!("Starting F* MCP server over stdio");
    server.run(DiscoveryAwareStdioTransport::new()).await?;
    sweeper_handle.abort();
    SESSION_MANAGER.close_all().await;
    Ok(())
}
