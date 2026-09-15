#![allow(clippy::result_large_err)]

pub mod cli;
pub mod config;
pub mod error;
pub mod lobsterai;
pub mod models;
pub mod observability;
pub mod redaction;
pub mod server;
pub mod session;
pub mod shutdown;
pub mod stream_watch;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The single upstream model this proxy serves.
pub const DEFAULT_MODEL: &str = "deepseek-v4.1-flash";
