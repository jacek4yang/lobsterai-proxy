//! Interactive browser login: local callback server on 127.0.0.1, portal
//! login URL, OAuth code exchange, credential persistence.
//!
//! Flow (mirrors the desktop client / reference login tool):
//! 1. bind a random local port, generate state + install uuid;
//! 2. open the portal login URL with source=electron and the callback
//!    redirect_uri;
//! 3. the browser redirects back with code + state after login;
//! 4. POST /api/auth/exchange with the code -> token + account payload;
//! 5. save `auth/lobsterai-<uid>.json` (nested form).

use reqwest::Client;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

use super::credential::{
    atomic_write, jwt_expiry, now_secs, parse_credential, CredentialData, CredentialError,
};
use crate::config::Config;

const CALLBACK_PATH: &str = "/auth/callback";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(600);
const EXCHANGE_PATH: &str = "/api/auth/exchange";

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("login timed out or was cancelled before the browser callback arrived")]
    Timeout,
    #[error("browser callback carried an invalid state parameter")]
    StateMismatch,
    #[error("browser callback carried no authorization code")]
    MissingCode,
    #[error("exchange rejected: {0}")]
    Exchange(String),
    #[error("exchange response carried no accessToken")]
    MissingToken,
    #[error("network error: {0}")]
    Network(String),
    #[error("credential validation failed: {0}")]
    Invalid(#[from] CredentialError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Build the login URL for the portal.
fn login_url(portal: &str, port: u16, state: &str) -> String {
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    format!(
        "{portal}/portal#/login?source=electron&redirect_uri={}&state={state}",
        urlencode(&redirect_uri)
    )
}

/// Minimal percent-encoding for query values (the redirect URI). `:` and `/`
/// are safe in a query value and kept; other separators are encoded.
fn urlencode(value: &str) -> String {
    const HEX: &[u8] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(byte as char)
            }
            _ => {
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// Exchange the auth code for the token + account payload.
pub async fn exchange(
    http: &Client,
    base_url: &str,
    code: &str,
    uuid: &str,
    first_keyfrom: &str,
) -> Result<Value, OAuthError> {
    let url = format!("{base_url}{EXCHANGE_PATH}");
    let body = json!({
        "authCode": code,
        "firstKeyfrom": first_keyfrom,
        "latestKeyfrom": format!("{}", now_millis()),
        "uuid": uuid,
        "version": super::credential::CLIENT_VERSION,
    });
    let response = http
        .post(&url)
        .headers(super::headers::auth_headers())
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| OAuthError::Network(e.to_string()))?;
    let status = response.status().as_u16();
    let raw = response.bytes().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(OAuthError::Exchange(format!(
            "HTTP {status}: {}",
            crate::redaction::sanitize_text(&String::from_utf8_lossy(&raw), &[])
        )));
    }
    let payload: Value = serde_json::from_slice(&raw)
        .map_err(|_| OAuthError::Exchange("response was not valid JSON".into()))?;
    let code_value = payload.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code_value != 0 {
        let msg = payload
            .get("msg")
            .or_else(|| payload.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(OAuthError::Exchange(format!("code={code_value}: {msg}")));
    }
    Ok(payload.get("data").cloned().unwrap_or(json!({})))
}

/// Assemble the exchange payload into the persisted credential file.
pub fn build_credential_file(exchange_data: &Value, uuid: &str, first_keyfrom: &str) -> Value {
    let access_token = exchange_data
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let refresh_token = exchange_data
        .get("refreshToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let expires_at = exchange_data
        .get("expiresIn")
        .and_then(Value::as_i64)
        .filter(|v| *v > 0)
        .map(|secs| now_secs() + secs)
        .or_else(|| jwt_expiry(&access_token))
        .unwrap_or(0);
    let user = exchange_data.get("user").cloned().unwrap_or(json!({}));
    let uid = ["id", "userId", "yid"]
        .iter()
        .find_map(|key| user.get(*key).and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .unwrap_or_default()
        .to_owned();
    json!({
        "auth": {
            "accessToken": access_token,
            "refreshToken": refresh_token,
            "expiresAt": expires_at,
            "uuid": uuid,
            "firstKeyfrom": first_keyfrom,
            "latestKeyfrom": format!("{}", now_millis()),
        },
        "account": {
            "uid": uid,
            "userId": user.get("userId").and_then(Value::as_str).unwrap_or_default(),
            "nickname": user.get("nickname").and_then(Value::as_str).unwrap_or_default(),
        },
    })
}

/// Validate and persist a credential into the managed auth dir. Returns the
/// destination file name (`lobsterai-<uid>.json`).
pub fn save_credential(config: &Config, credential_value: Value) -> Result<String, OAuthError> {
    let parsed = parse_credential(credential_value.clone())?;
    let dir = &config.auth.dir;
    std::fs::create_dir_all(dir)?;
    let name = format!("lobsterai-{}.json", parsed.uid);
    let dest = dir.join(&name);
    atomic_write(&dest, &credential_value).map_err(OAuthError::Network)?;
    Ok(name)
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn browser_url_allowed(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https"))
}

/// Best-effort browser open across platforms. The URL never enters shell
/// syntax (Windows uses the system URL protocol handler directly).
pub fn open_browser(url: &str) -> bool {
    if !browser_url_allowed(url) {
        return false;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("rundll32.exe")
            .arg("url.dll,FileProtocolHandler")
            .arg(url)
            .spawn()
            .is_ok()
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn().is_ok()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .is_ok()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    {
        false
    }
}

fn urldecode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Handle one browser callback connection: parse the query, validate the
/// state, respond with a friendly page, and return the code.
async fn handle_callback(
    mut stream: tokio::net::TcpStream,
    state_key: String,
) -> Result<String, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buffer = Vec::with_capacity(2048);
    loop {
        let mut chunk = [0u8; 1024];
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                if buffer.windows(4).any(|w| w == b"\r\n\r\n") || buffer.len() > 16384 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let request_line = String::from_utf8_lossy(&buffer);
    let target = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let query = target.split('?').nth(1).unwrap_or_default();
    let mut code = String::new();
    let mut got_state = String::new();
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "code" => code = urldecode(value),
            "state" => got_state = urldecode(value),
            _ => {}
        }
    }
    let (head, outcome): (&str, Result<String, String>) = if code.is_empty() {
        (
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<h2>login callback invalid</h2>",
            Err("missing code".into()),
        )
    } else if got_state != state_key {
        (
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<h2>state mismatch</h2>",
            Err("state mismatch".into()),
        )
    } else {
        (
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: 47\r\n\r\n<h2>login ok - you can close this window</h2>",
            Ok(code),
        )
    };
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.flush().await;
    outcome
}

/// Full interactive login: local callback server, portal URL, exchange, save.
pub async fn run_login(config: &Config, open_browser_flag: bool) -> Result<PathBuf, OAuthError> {
    let state_key: String = {
        use rand::Rng;
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    };
    let uuid = new_uuid();
    let first_keyfrom = format!("{}", now_millis());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| OAuthError::Network(format!("bind callback server: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| OAuthError::Network(e.to_string()))?
        .port();
    let url = login_url(&config.upstream.login_portal, port, &state_key);

    println!("Please open this link in your browser to log in:");
    println!("{url}");
    if open_browser_flag && !open_browser(&url) {
        println!("Could not open a browser automatically; open the link above manually.");
    }
    println!("Waiting for the browser callback (10 minutes)...");

    // Accept connections until one yields a valid code.
    let accept = async {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return Err(OAuthError::Network("callback listener closed".into()));
            };
            if let Ok(outcome) = handle_callback(stream, state_key.clone()).await {
                return Ok(outcome);
            }
            // Invalid callback (favicon request, user reload, …): keep waiting.
        }
    };
    let code = tokio::select! {
        biased;
        result = accept => result.map_err(|_: OAuthError| OAuthError::Timeout)?,
        _ = tokio::time::sleep(CALLBACK_TIMEOUT) => return Err(OAuthError::Timeout),
    };
    if code.is_empty() {
        return Err(OAuthError::MissingCode);
    }

    let http = Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| OAuthError::Network(e.to_string()))?;
    let exchange_data = exchange(
        &http,
        &config.upstream.base_url,
        &code,
        &uuid,
        &first_keyfrom,
    )
    .await?;
    if exchange_data
        .get("accessToken")
        .and_then(Value::as_str)
        .is_none()
    {
        return Err(OAuthError::MissingToken);
    }
    let credential_value = build_credential_file(&exchange_data, &uuid, &first_keyfrom);
    let name = save_credential(config, credential_value.clone())?;
    let path = config.auth.dir.join(&name);

    let parsed: CredentialData = parse_credential(credential_value)?;
    println!("Login ok. Credential saved to: {}", path.display());
    println!(
        "Account: {} (uid length {})",
        parsed.nickname,
        parsed.uid.len()
    );
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_url_embeds_redirect_and_state() {
        let url = login_url("https://portal.example.com", 51234, "abc123");
        assert!(url.starts_with("https://portal.example.com/portal#/login?source=electron"));
        assert!(url.contains("redirect_uri=http://127.0.0.1:51234/auth/callback"));
        assert!(url.ends_with("state=abc123"));
    }

    #[test]
    fn urlencode_encodes_separators_only() {
        // `:` and `/` are safe in a query value and kept; `?`, `&`, `=` are
        // percent-encoded.
        assert_eq!(
            urlencode("http://h/a?b=1&c=2"),
            "http://h/a%3Fb%3D1%26c%3D2"
        );
        assert_eq!(urlencode("plain"), "plain");
    }

    #[test]
    fn urldecode_roundtrips() {
        assert_eq!(urldecode("%3A%2F%2F"), "://");
        assert_eq!(urldecode("a+b"), "a b");
        assert_eq!(urldecode("plain"), "plain");
    }

    #[test]
    fn build_credential_file_shapes_nested_form() {
        let exchange = json!({
            "accessToken": "eyJhbGciOiJIUzUxMiJ9.eyJleHAiOjE5MDAwMDAwMDB9.sig",
            "refreshToken": "rt",
            "expiresIn": 3600,
            "user": {"id": "u-9", "userId": "yid-9", "nickname": "nick"}
        });
        let value = build_credential_file(&exchange, "u-u-i-d", "1700000000000");
        assert_eq!(value["account"]["uid"], "u-9");
        assert_eq!(value["auth"]["uuid"], "u-u-i-d");
        assert_eq!(value["auth"]["expiresAt"], now_secs() + 3600);
        assert!(parse_credential(value).is_ok());
    }

    #[test]
    fn build_credential_file_falls_back_to_jwt_exp() {
        let exchange = json!({
            "accessToken": "h.eyJleHAiOjE5MDAwMDAwMDB9.s",
            "refreshToken": "rt",
            "user": {"id": "u-1"}
        });
        let value = build_credential_file(&exchange, "uuid", "1700000000000");
        assert_eq!(value["auth"]["expiresAt"], 1_900_000_000);
    }

    #[test]
    fn browser_url_policy_accepts_only_http_and_https() {
        assert!(browser_url_allowed(
            "https://portal.example.com/portal#/login"
        ));
        assert!(!browser_url_allowed("file:///tmp/token"));
        assert!(!browser_url_allowed("javascript:alert(1)"));
    }

    #[test]
    fn save_credential_validates_before_writing() {
        let mut config = crate::config::Config::default();
        let dir = std::env::temp_dir().join(format!("lap-oauth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        config.auth.dir = dir.clone();
        let bad = json!({"auth": {"accessToken": ""}});
        assert!(save_credential(&config, bad).is_err());
        let good = build_credential_file(
            &json!({
                "accessToken": "tok",
                "refreshToken": "rt",
                "user": {"id": "u-save"}
            }),
            "uuid",
            "1700000000000",
        );
        let name = save_credential(&config, good.clone()).unwrap();
        assert_eq!(name, "lobsterai-u-save.json");
        assert!(dir.join(&name).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
