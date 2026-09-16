//! Entry point: CLI dispatch (serve / login / status), tracing setup,
//! account pool bootstrap, housekeeping spawn.

use anyhow::{Context, Result};

use lobsterai_proxy::cli::{Cli, Command};

use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = lobsterai_proxy::config::load(cli.config.as_deref()).context("loading config")?;

    install_tracing(&config.log_level);

    match &cli.command {
        Command::Login { no_browser } => {
            lobsterai_proxy::lobsterai::oauth::run_login(&config, !*no_browser).await?;
            Ok(())
        }
        Command::Status => {
            let state = lobsterai_proxy::server::build_state(config)?;
            lobsterai_proxy::server::print_status(&state).await;
            Ok(())
        }
        Command::Checkin => {
            let state = lobsterai_proxy::server::build_state(config)?;
            lobsterai_proxy::lobsterai::checkin::checkin_all(
                &state.pool,
                &state.http,
                &state.config.upstream.base_url,
            )
            .await;
            Ok(())
        }
        Command::Serve { host, port } => {
            let mut config = config;
            if let Some(host) = host {
                config.server.host = host.clone();
            }
            if let Some(port) = port {
                config.server.port = *port;
            }
            lobsterai_proxy::config::validate_public(&config)
                .context("host/port override validation")?;
            lobsterai_proxy::server::serve(config).await
        }
    }
}

fn install_tracing(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_thread_names(false)
        .compact()
        .init();
}

/// Keep the binary surface minimal; timing constants live in the server module.
#[allow(dead_code)]
fn _unused(_: std::time::Duration) {}
