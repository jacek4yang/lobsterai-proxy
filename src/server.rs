//! Placeholder server module for the scaffold PR. The full router,
//! orchestrator and state bootstrap arrive in the following PRs; this keeps
//! the `serve`/`status` CLI surface compiling end to end.

use crate::config::Config;
use anyhow::Context as _;

/// Server state shared across handlers (populated by `build_state`).
pub struct AppState {
    pub config: Config,
    pub started_at: std::time::Instant,
}

/// Build the server state from config: load accounts, HTTP client, metrics.
pub fn build_state(config: Config) -> Result<AppState, anyhow::Error> {
    Ok(AppState {
        config,
        started_at: std::time::Instant::now(),
    })
}

/// Human-readable status for the `status` subcommand (no secrets).
pub fn print_status(state: &AppState) {
    println!("model      : {}", state.config.model.default);
    println!("auth dir   : {}", state.config.auth.dir.display());
    println!("upstream   : {}", state.config.upstream.base_url);
    println!("accounts   : (pool arrives in the next PR)");
}

/// Start the proxy server (router arrives in the next PR; this is a stub
/// that binds and reports, so the deployment shape is testable early).
pub async fn serve(config: Config) -> Result<(), anyhow::Error> {
    let addr = format!("{}:{}", config.server.host, config.server.port);
    let _listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    anyhow::bail!("router not yet implemented (scaffold build)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_state_loads_defaults() {
        let state = build_state(Config::default()).unwrap();
        assert_eq!(state.config.model.default, "deepseek-v4.1-flash");
    }
}
