//! Configuration: built-in defaults, optional TOML file, environment overrides.

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub model: ModelConfig,
    pub upstream: UpstreamConfig,
    pub auth: AuthConfig,
    pub timeouts: TimeoutConfig,
    pub limits: LimitConfig,
    pub checkin: CheckinConfig,
    pub log_level: String,
    /// Optional fixed server secret (hex). Absent → generated per process.
    pub secret: Option<SecretString>,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// Local API key. Must be non-empty when bound to a non-loopback address.
    pub api_key: Option<SecretString>,
}

#[derive(Debug, Clone)]
pub struct ModelConfig {
    /// The single upstream model this build proxies.
    pub default: String,
}

#[derive(Debug, Clone)]
pub struct UpstreamConfig {
    /// LobsterAI backend origin (no trailing slash).
    pub base_url: String,
    /// LobsterAI login portal origin (browser OAuth).
    pub login_portal: String,
    /// Optional outbound proxy, e.g. `socks5h://127.0.0.1:10808`.
    /// Empty → deterministic direct connection (env proxies ignored).
    pub proxy: Option<String>,
    pub connect_timeout_secs: u64,
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// Directory holding `lobsterai-*.json` credential files and persisted state.
    pub dir: PathBuf,
    /// Extra read-only directories to seed credentials from (copied into `dir`).
    pub seed_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
pub struct TimeoutConfig {
    pub first_event_secs: u64,
    pub first_semantic_secs: u64,
    pub stream_idle_secs: u64,
    pub semantic_idle_secs: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct LimitConfig {
    pub shadow_max_sessions: usize,
    pub shadow_ttl_secs: u64,
    pub sticky_ttl_secs: u64,
    pub sticky_max: usize,
    pub cred_cooldown_secs: u64,
    /// Hard cooldown after an out-of-credits failure.
    pub hard_credit_cooldown_secs: u64,
    pub model_cooldown_secs: u64,
    pub model_cooldown_max_secs: u64,
    /// Refresh this many seconds before `expiresAt`.
    pub refresh_margin_secs: u64,
    /// Daily keepalive refresh interval for idle refresh tokens.
    pub keepalive_secs: u64,
    /// Maximum accepted request body bytes.
    pub max_body_bytes: usize,
    /// How long `serve` waits for in-flight requests after a shutdown signal
    /// before abandoning them. `0` means "drain immediately" (never wait).
    pub shutdown_timeout_secs: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckinConfig {
    pub enabled: bool,
    pub interval_secs: u64,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            first_event_secs: 90,
            first_semantic_secs: 240,
            stream_idle_secs: 120,
            semantic_idle_secs: 300,
        }
    }
}

impl Default for LimitConfig {
    fn default() -> Self {
        Self {
            shadow_max_sessions: 256,
            shadow_ttl_secs: 600,
            sticky_ttl_secs: 1800,
            sticky_max: 512,
            cred_cooldown_secs: 300,
            hard_credit_cooldown_secs: 12 * 3600,
            model_cooldown_secs: 600,
            model_cooldown_max_secs: 86400,
            refresh_margin_secs: 600,
            keepalive_secs: 86400,
            max_body_bytes: 100 * 1024 * 1024,
            shutdown_timeout_secs: 30,
        }
    }
}

impl Default for CheckinConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 3600,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 8090,
                api_key: None,
            },
            model: ModelConfig {
                default: crate::DEFAULT_MODEL.into(),
            },
            upstream: UpstreamConfig {
                base_url: "https://lobsterai-server.youdao.com".into(),
                login_portal: "https://lobsterai.youdao.com".into(),
                proxy: None,
                connect_timeout_secs: 15,
            },
            auth: AuthConfig {
                dir: default_auth_dir(),
                seed_dirs: Vec::new(),
            },
            timeouts: TimeoutConfig::default(),
            limits: LimitConfig::default(),
            checkin: CheckinConfig::default(),
            log_level: "info".into(),
            secret: None,
        }
    }
}

// --- TOML surface (all optional; missing keys fall back to defaults) ---

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    server: FileServer,
    model: FileModel,
    upstream: FileUpstream,
    auth: FileAuth,
    timeouts: FileTimeouts,
    limits: FileLimits,
    checkin: FileCheckin,
    log_level: Option<String>,
    secret: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileServer {
    host: Option<String>,
    port: Option<u16>,
    api_key: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileModel {
    default: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileUpstream {
    base_url: Option<String>,
    login_portal: Option<String>,
    proxy: Option<String>,
    connect_timeout_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileAuth {
    dir: Option<String>,
    seed_dirs: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileTimeouts {
    first_event_secs: Option<u64>,
    first_semantic_secs: Option<u64>,
    stream_idle_secs: Option<u64>,
    semantic_idle_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileLimits {
    shadow_max_sessions: Option<usize>,
    shadow_ttl_secs: Option<u64>,
    sticky_ttl_secs: Option<u64>,
    sticky_max: Option<usize>,
    cred_cooldown_secs: Option<u64>,
    hard_credit_cooldown_secs: Option<u64>,
    model_cooldown_secs: Option<u64>,
    model_cooldown_max_secs: Option<u64>,
    refresh_margin_secs: Option<u64>,
    keepalive_secs: Option<u64>,
    max_body_bytes: Option<usize>,
    shutdown_timeout_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileCheckin {
    enabled: Option<bool>,
    interval_secs: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

/// Load config: defaults ← TOML file (if provided or ./config.toml exists) ← env overrides.
pub fn load(explicit_path: Option<&Path>) -> Result<Config, ConfigError> {
    let path = match explicit_path {
        Some(p) => Some(p.to_path_buf()),
        None => {
            let default = PathBuf::from("config.toml");
            default.exists().then_some(default)
        }
    };
    let mut config = Config::default();
    if let Some(path) = &path {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        let file: FileConfig = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;
        apply_file(&mut config, file);
    }
    apply_env(&mut config);
    validate(&config)?;
    Ok(config)
}

fn apply_file(config: &mut Config, file: FileConfig) {
    if let Some(host) = file.server.host {
        config.server.host = host;
    }
    if let Some(port) = file.server.port {
        config.server.port = port;
    }
    if let Some(api_key) = file.server.api_key {
        if !api_key.is_empty() {
            config.server.api_key = Some(SecretString::from(api_key));
        }
    }
    if let Some(model) = file.model.default {
        config.model.default = model;
    }
    if let Some(base) = file.upstream.base_url {
        config.upstream.base_url = base.trim_end_matches('/').to_owned();
    }
    if let Some(portal) = file.upstream.login_portal {
        config.upstream.login_portal = portal.trim_end_matches('/').to_owned();
    }
    if let Some(proxy) = file.upstream.proxy {
        config.upstream.proxy = (!proxy.is_empty()).then_some(proxy);
    }
    if let Some(secs) = file.upstream.connect_timeout_secs {
        config.upstream.connect_timeout_secs = secs;
    }
    if let Some(dir) = file.auth.dir {
        config.auth.dir = PathBuf::from(dir);
    }
    if let Some(dirs) = file.auth.seed_dirs {
        config.auth.seed_dirs = dirs.iter().map(PathBuf::from).collect();
    }
    let t = &file.timeouts;
    if let Some(v) = t.first_event_secs {
        config.timeouts.first_event_secs = v;
    }
    if let Some(v) = t.first_semantic_secs {
        config.timeouts.first_semantic_secs = v;
    }
    if let Some(v) = t.stream_idle_secs {
        config.timeouts.stream_idle_secs = v;
    }
    if let Some(v) = t.semantic_idle_secs {
        config.timeouts.semantic_idle_secs = v;
    }
    let l = &file.limits;
    if let Some(v) = l.shadow_max_sessions {
        config.limits.shadow_max_sessions = v;
    }
    if let Some(v) = l.shadow_ttl_secs {
        config.limits.shadow_ttl_secs = v;
    }
    if let Some(v) = l.sticky_ttl_secs {
        config.limits.sticky_ttl_secs = v;
    }
    if let Some(v) = l.sticky_max {
        config.limits.sticky_max = v;
    }
    if let Some(v) = l.cred_cooldown_secs {
        config.limits.cred_cooldown_secs = v;
    }
    if let Some(v) = l.hard_credit_cooldown_secs {
        config.limits.hard_credit_cooldown_secs = v;
    }
    if let Some(v) = l.model_cooldown_secs {
        config.limits.model_cooldown_secs = v;
    }
    if let Some(v) = l.model_cooldown_max_secs {
        config.limits.model_cooldown_max_secs = v;
    }
    if let Some(v) = l.refresh_margin_secs {
        config.limits.refresh_margin_secs = v;
    }
    if let Some(v) = l.keepalive_secs {
        config.limits.keepalive_secs = v;
    }
    if let Some(v) = l.max_body_bytes {
        config.limits.max_body_bytes = v;
    }
    if let Some(v) = l.shutdown_timeout_secs {
        config.limits.shutdown_timeout_secs = v;
    }
    if let Some(v) = file.checkin.enabled {
        config.checkin.enabled = v;
    }
    if let Some(v) = file.checkin.interval_secs {
        config.checkin.interval_secs = v;
    }
    if let Some(level) = file.log_level {
        config.log_level = level;
    }
    if let Some(secret) = file.secret {
        config.secret = Some(SecretString::from(secret));
    }
}

fn apply_env(config: &mut Config) {
    if let Ok(dir) = std::env::var("LOBSTERAI_AUTH_DIR") {
        if !dir.is_empty() {
            config.auth.dir = PathBuf::from(dir);
        }
    }
    if let Ok(key) = std::env::var("LOBSTERAI_PROXY_API_KEY") {
        if !key.is_empty() {
            config.server.api_key = Some(SecretString::from(key));
        }
    }
    if let Ok(base) = std::env::var("LOBSTERAI_UPSTREAM_BASE") {
        if !base.is_empty() {
            config.upstream.base_url = base.trim_end_matches('/').to_owned();
        }
    }
    if let Ok(portal) = std::env::var("LOBSTERAI_LOGIN_PORTAL") {
        if !portal.is_empty() {
            config.upstream.login_portal = portal.trim_end_matches('/').to_owned();
        }
    }
    if let Ok(proxy) = std::env::var("LOBSTERAI_PROXY_URL") {
        if !proxy.is_empty() {
            config.upstream.proxy = Some(proxy);
        }
    }
}

fn validate(config: &Config) -> Result<(), ConfigError> {
    let is_loopback = config.server.host == "127.0.0.1"
        || config.server.host == "::1"
        || config.server.host == "localhost";
    if !is_loopback && config.server.api_key.is_none() {
        return Err(ConfigError::Invalid(
            "api_key must be set when binding a non-loopback address \
             (otherwise the LobsterAI accounts are exposed to the network)"
                .into(),
        ));
    }
    if config.upstream.base_url.is_empty() {
        return Err(ConfigError::Invalid(
            "upstream.base_url must not be empty".into(),
        ));
    }
    Ok(())
}

/// Public validation hook for CLI host/port overrides.
pub fn validate_public(config: &Config) -> Result<(), ConfigError> {
    validate(config)
}

impl Config {
    pub fn server_secret(&self) -> Vec<u8> {
        match &self.secret {
            Some(hex_secret) => {
                let trimmed = hex_secret.expose_secret().trim();
                match decode_hex(trimmed) {
                    Ok(bytes) if bytes.len() >= 16 => bytes,
                    _ => trimmed.as_bytes().to_vec(),
                }
            }
            None => {
                use rand::Rng;
                let mut key = [0u8; 32];
                rand::rng().fill_bytes(&mut key);
                key.to_vec()
            }
        }
    }

    pub fn timeouts(&self) -> crate::stream_watch::StreamTimeouts {
        let t = self.timeouts;
        crate::stream_watch::StreamTimeouts::from_secs(
            t.first_event_secs,
            t.first_semantic_secs,
            t.stream_idle_secs,
            t.semantic_idle_secs,
        )
    }

    pub fn shadow_ttl(&self) -> Duration {
        Duration::from_secs(self.limits.shadow_ttl_secs)
    }

    /// Bounded drain budget for graceful shutdown (`[limits]
    /// shutdown_timeout_secs`, default 30). An explicit `0` disables the
    /// wait: the process exits as soon as the listener stops accepting.
    pub fn shutdown_timeout(&self) -> Duration {
        Duration::from_secs(self.limits.shutdown_timeout_secs)
    }
}

/// Default credential/state directory: `auth/` next to the running executable
/// (portable deployment), falling back to `./auth` when the exe path is not
/// usable. An explicit `auth.dir` config or `LOBSTERAI_AUTH_DIR` overrides it.
fn default_auth_dir() -> PathBuf {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    if let Some(dir) = exe {
        let candidate = dir.join("auth");
        if std::fs::create_dir_all(&candidate).is_ok() {
            return candidate;
        }
    }
    PathBuf::from("auth")
}

fn decode_hex(text: &str) -> Result<Vec<u8>, ()> {
    if !text.len().is_multiple_of(2) {
        return Err(());
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|_| ()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_target_deepseek_v41_flash_on_loopback() {
        let config = Config::default();
        assert_eq!(config.model.default, "deepseek-v4.1-flash");
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 8090);
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn non_loopback_requires_api_key() {
        let mut config = Config::default();
        config.server.host = "0.0.0.0".into();
        assert!(validate(&config).is_err());
        config.server.api_key = Some(SecretString::from("k"));
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn server_secret_falls_back_to_raw_ascii_when_not_hex() {
        let config = Config {
            secret: Some(SecretString::from("zzzz-not-hex")),
            ..Config::default()
        };
        let secret = config.server_secret();
        assert_eq!(secret, b"zzzz-not-hex".to_vec());
        let config = Config {
            secret: Some(SecretString::from("00112233445566778899aabbccddeeff")),
            ..Config::default()
        };
        assert_eq!(
            config.server_secret(),
            [
                0x00u8, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc,
                0xdd, 0xee, 0xff
            ]
            .to_vec()
        );
    }

    #[test]
    fn file_config_overrides_defaults() {
        let text = r#"
[server]
host = "192.168.1.5"
port = 9090
api_key = "sk"

[model]
default = "deepseek-v4.1-flash"

[upstream]
base_url = "https://lobsterai-server.youdao.com/"
proxy = "socks5h://127.0.0.1:10808"

[checkin]
enabled = false
"#;
        let file: FileConfig = toml::from_str(text).unwrap();
        let mut config = Config::default();
        apply_file(&mut config, file);
        assert_eq!(config.server.host, "192.168.1.5");
        assert_eq!(config.server.port, 9090);
        assert_eq!(
            config.upstream.base_url,
            "https://lobsterai-server.youdao.com"
        );
        assert_eq!(
            config.upstream.proxy.as_deref(),
            Some("socks5h://127.0.0.1:10808")
        );
        assert!(!config.checkin.enabled);
    }

    #[test]
    fn env_overrides_apply() {
        let mut config = Config::default();
        std::env::set_var("LOBSTERAI_AUTH_DIR", "custom-auth");
        std::env::set_var("LOBSTERAI_PROXY_API_KEY", "sk-env");
        std::env::set_var("LOBSTERAI_UPSTREAM_BASE", "https://mirror.example.com/");
        apply_env(&mut config);
        std::env::remove_var("LOBSTERAI_AUTH_DIR");
        std::env::remove_var("LOBSTERAI_PROXY_API_KEY");
        std::env::remove_var("LOBSTERAI_UPSTREAM_BASE");
        assert_eq!(config.auth.dir, PathBuf::from("custom-auth"));
        assert_eq!(
            config.server.api_key.map(|k| k.expose_secret().to_owned()),
            Some("sk-env".into())
        );
        assert_eq!(config.upstream.base_url, "https://mirror.example.com");
    }

    #[test]
    fn shutdown_timeout_defaults_to_30s_and_is_configurable() {
        assert_eq!(
            Config::default().shutdown_timeout(),
            Duration::from_secs(30),
            "the documented default drain budget"
        );
        let text = "[limits]\nshutdown_timeout_secs = 5\n";
        let file: FileConfig = toml::from_str(text).unwrap();
        let mut config = Config::default();
        apply_file(&mut config, file);
        assert_eq!(config.shutdown_timeout(), Duration::from_secs(5));
        // An explicit 0 is honoured (drain disabled, not "unset").
        let text = "[limits]\nshutdown_timeout_secs = 0\n";
        let file: FileConfig = toml::from_str(text).unwrap();
        let mut config = Config::default();
        apply_file(&mut config, file);
        assert_eq!(config.shutdown_timeout(), Duration::ZERO);
    }
}
