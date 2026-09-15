//! CLI definitions for `lobsterai-proxy`.

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "lobsterai-proxy",
    version,
    about = "Local Anthropic + Responses API proxy backed by LobsterAI deepseek-v4.1-flash"
)]
pub struct Cli {
    /// Path to the config file (default: ./config.toml if present, built-in defaults otherwise).
    #[arg(long, global = true)]
    pub config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Start the proxy server.
    Serve {
        /// Bind address override.
        #[arg(long)]
        host: Option<String>,
        /// Port override.
        #[arg(long, short)]
        port: Option<u16>,
    },
    /// Interactive browser login: opens the LobsterAI portal, waits for the
    /// local callback, exchanges the code and saves the credential.
    Login {
        /// Only print the login URL, do not open a browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// Check the account pool, check-in and credit status without starting the server.
    Status,
}
