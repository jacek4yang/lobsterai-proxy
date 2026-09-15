//! LobsterAI credentials: parsing (nested/flat forms), secrecy, single-flight
//! refresh gate, and atomic persistence. Credential files use the shape the
//! official client and the reference login tool write:
//!
//! nested: `{"auth": {...}, "account": {...}}`
//! flat:   `{"accessToken": ..., "uid": ...}` (accepted read-only)

use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{json, Value};
use sha2::Sha256;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

/// The LobsterAI backend origin.
pub const SERVER_BASE: &str = "https://lobsterai-server.youdao.com";
/// Official desktop client version gate. The check-in endpoints return
/// `slotState=empty` for any lower version, so this value is load-bearing.
pub const CLIENT_VERSION: &str = "2026.9.4";
/// Chat request User-Agent / version header value.
pub const USER_AGENT: &str = "LobsterAI/2026.9.4";
/// Advertised client capabilities for chat requests.
pub const CLIENT_CAPABILITIES: &str = "kimi-k3-agentic-v1";

#[derive(thiserror::Error)]
pub enum CredentialError {
    #[error("credential is not a valid JSON object")]
    NotAnObject,
    #[error("credential is missing accessToken")]
    MissingAccessToken,
    #[error("credential file too large (max 1 MiB)")]
    TooLarge,
    #[error("failed to read credential file")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse credential: invalid UTF-8/JSON")]
    Parse { path: PathBuf },
}

impl std::fmt::Debug for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

/// Parsed credential payload. Secrets are wrapped; `Debug` never exposes them.
pub struct CredentialData {
    /// Full credential JSON as parsed from disk (re-serialized on save).
    pub raw: Value,
    pub uid: String,
    pub user_id: String,
    pub nickname: String,
    pub uuid: String,
    pub first_keyfrom: String,
    pub latest_keyfrom: String,
    pub access_token: SecretString,
    pub refresh_token: SecretString,
    /// Seconds since epoch; 0 when unknown (treated as expired).
    pub expires_at_secs: i64,
    pub last_refresh_secs: i64,
}

impl std::fmt::Debug for CredentialData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialData")
            .field("uid_len", &self.uid.len())
            .field("expires_at_secs", &self.expires_at_secs)
            .field(
                "has_refresh_token",
                &(!self.refresh_token.expose_secret().is_empty()),
            )
            .finish_non_exhaustive()
    }
}

fn str_field(object: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| object.get(*k).and_then(Value::as_str))
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Validate and parse the raw credential JSON (nested or flat form).
pub fn parse_credential(raw: Value) -> Result<CredentialData, CredentialError> {
    let object = raw.as_object().ok_or(CredentialError::NotAnObject)?;
    let (auth, account) = if object.contains_key("auth") {
        (
            object.get("auth").cloned().unwrap_or(json!({})),
            object.get("account").cloned().unwrap_or(json!({})),
        )
    } else {
        // Flat form: every field lives at the top level.
        (raw.clone(), raw.clone())
    };
    let access_token = str_field(&auth, &["accessToken", "access_token", "token"]);
    if access_token.is_empty() {
        return Err(CredentialError::MissingAccessToken);
    }
    let uid = str_field(&account, &["uid", "id"]);
    // Flat form stores uid at the top level of `raw`, not `account`.
    let uid = if uid.is_empty() && object.contains_key("uid") {
        str_field(&raw, &["uid"])
    } else {
        uid
    };
    let expires_at_secs =
        norm_ts(auth.get("expiresAt").or_else(|| auth.get("expires_at"))).unwrap_or(0);
    Ok(CredentialData {
        raw,
        uid,
        user_id: str_field(&account, &["userId", "user_id", "yid"]),
        nickname: str_field(&account, &["nickname"]),
        uuid: str_field(&auth, &["uuid"]),
        first_keyfrom: str_field(&auth, &["firstKeyfrom"]),
        latest_keyfrom: str_field(&auth, &["latestKeyfrom"]),
        access_token: SecretString::from(access_token),
        refresh_token: SecretString::from(str_field(&auth, &["refreshToken", "refresh_token"])),
        expires_at_secs,
        last_refresh_secs: norm_ts(auth.get("lastRefreshTime")).unwrap_or(0),
    })
}

/// Read and parse a credential file (1 MiB cap, regular files only).
pub fn read_credential_file(path: &Path) -> Result<CredentialData, CredentialError> {
    let meta = std::fs::metadata(path).map_err(|source| CredentialError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !meta.is_file() || meta.len() > 1024 * 1024 {
        return Err(CredentialError::TooLarge);
    }
    let bytes = std::fs::read(path).map_err(|source| CredentialError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| CredentialError::Parse {
        path: path.to_path_buf(),
    })?;
    parse_credential(value).map_err(|e| match e {
        CredentialError::NotAnObject | CredentialError::MissingAccessToken => {
            CredentialError::Parse {
                path: path.to_path_buf(),
            }
        }
        other => other,
    })
}

/// Opaque per-account display name: first 8 hex chars of HMAC(secret, uid).
/// Deterministic, stable across restarts with a fixed server secret, and
/// never contains the raw uid.
pub fn safe_name(server_secret: &[u8], uid: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(server_secret).expect("HMAC accepts any key length");
    mac.update(b"lobsterai-proxy/cred-safe-name/v1\0");
    mac.update(uid.as_bytes());
    let digest = mac.finalize().into_bytes();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("acct-{}", &hex[..8])
}

/// A live credential handle shared across requests. Refresh is single-flight:
/// concurrent expiries share one refresh call.
pub struct Credential {
    /// Stable internal id (file name).
    pub id: String,
    pub safe_name: String,
    pub path: PathBuf,
    data: std::sync::RwLock<CredentialData>,
    /// Held while a refresh HTTP call is in flight (single-flight gate).
    pub(crate) refresh_lock: tokio::sync::Mutex<()>,
}

impl Credential {
    pub fn new(path: PathBuf, data: CredentialData, server_secret: &[u8]) -> Arc<Self> {
        let id = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown.json".into());
        let safe_name = safe_name(server_secret, &data.uid);
        Arc::new(Self {
            id,
            safe_name,
            path,
            data: std::sync::RwLock::new(data),
            refresh_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn access_token(&self) -> String {
        self.data
            .read()
            .unwrap()
            .access_token
            .expose_secret()
            .to_owned()
    }

    pub fn refresh_token(&self) -> String {
        self.data
            .read()
            .unwrap()
            .refresh_token
            .expose_secret()
            .to_owned()
    }

    pub fn with_data<R>(&self, f: impl FnOnce(&CredentialData) -> R) -> R {
        f(&self.data.read().unwrap())
    }

    pub fn expires_at_secs(&self) -> i64 {
        self.data.read().unwrap().expires_at_secs
    }

    pub fn last_refresh_secs(&self) -> i64 {
        self.data.read().unwrap().last_refresh_secs
    }

    pub fn uid(&self) -> String {
        self.data.read().unwrap().uid.clone()
    }

    /// True when the access token expires within `margin_secs` or has no
    /// recorded expiry at all.
    pub fn needs_refresh(&self, margin_secs: i64, now_secs: i64) -> bool {
        let expires_at = self.expires_at_secs();
        expires_at == 0 || now_secs >= expires_at.saturating_sub(margin_secs)
    }

    /// Replace in-memory auth data and atomically persist to disk. The
    /// `account` section is preserved (refreshes never change it); `auth` is
    /// replaced with the refreshed payload, re-serialized in the nested form
    /// the login tool also reads.
    pub fn apply_refresh(
        &self,
        access_token: &str,
        refresh_token: &str,
        expires_at_secs: i64,
    ) -> Result<(), String> {
        let mut data = self.data.write().unwrap();
        let mut raw = data.raw.clone();
        // Re-read the file in case an external process updated it.
        if let Ok(bytes) = std::fs::read(&self.path) {
            if let Ok(current) = serde_json::from_slice::<Value>(&bytes) {
                raw = current;
            }
        }
        // Carry forward keyfrom fields the refresh endpoint requires.
        let old_auth = raw.get("auth").cloned().unwrap_or_else(|| {
            // Flat form: fields live at the top level.
            raw.clone()
        });
        let auth = json!({
            "accessToken": access_token,
            "refreshToken": refresh_token,
            "expiresAt": expires_at_secs,
            "uuid": str_field(&old_auth, &["uuid"]),
            "firstKeyfrom": str_field(&old_auth, &["firstKeyfrom"]),
            "latestKeyfrom": now_secs().to_string(),
            "lastRefreshTime": now_secs(),
        });
        if raw.get("account").is_some() || raw.get("auth").is_some() {
            raw["auth"] = auth;
        } else {
            // Flat form input: normalize to the nested form on first save.
            raw = json!({"auth": auth, "account": data_account_value(&data)});
        }
        atomic_write(&self.path, &raw)?;
        let parsed =
            parse_credential(raw).map_err(|e| format!("post-refresh reparse failed: {e}"))?;
        *data = parsed;
        Ok(())
    }

    /// Force in-memory expiry state to "expired" (used after a 401) without
    /// touching the file.
    pub fn mark_access_expired(&self) {
        let mut data = self.data.write().unwrap();
        data.expires_at_secs = now_secs();
    }
}

fn data_account_value(data: &CredentialData) -> Value {
    json!({
        "uid": data.uid,
        "userId": data.user_id,
        "nickname": data.nickname,
    })
}

/// Prefix of the temp file [`atomic_write`] creates next to the target.
///
/// Hidden name in the *same* directory so the publishing `rename` is atomic
/// on every supported platform. A crash between create and rename can only
/// leave an orphan temp file behind — never a partial credential.
pub const TEMP_FILE_PREFIX: &str = ".credential-";
/// Suffix of the temp file [`atomic_write`] creates.
pub const TEMP_FILE_SUFFIX: &str = ".tmp";

/// Remove exact-shaped credential temp files older than 24 hours.
pub fn cleanup_stale_temp_files(dir: &Path) -> usize {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0usize;
    for entry in read_dir.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(id) = name
            .strip_prefix(TEMP_FILE_PREFIX)
            .and_then(|n| n.strip_suffix(TEMP_FILE_SUFFIX))
        else {
            continue;
        };
        if id.len() != 16 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !entry.file_type().is_ok_and(|kind| kind.is_file())
            || !metadata
                .modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age >= std::time::Duration::from_secs(86400))
        {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                removed += 1;
                tracing::debug!("removed stale credential temp file");
            }
            Err(err) => {
                tracing::debug!(error = %crate::redaction::sanitize_text(&err.to_string(), &[]), "could not remove stale temp file")
            }
        }
    }
    removed
}

/// Atomic credential write: temp file in the same directory + rename.
pub fn atomic_write(path: &Path, value: &Value) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| "credential path has no parent".to_owned())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create auth dir: {e}"))?;
    let body = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(
        "{}{}{}",
        TEMP_FILE_PREFIX,
        crate::session::random_hex_16(),
        TEMP_FILE_SUFFIX
    ));
    {
        let mut file =
            std::fs::File::create(&tmp).map_err(|e| format!("write temp credential: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        file.write_all(&body)
            .map_err(|e| format!("write temp credential: {e}"))?;
        file.sync_all().ok();
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("replace credential file: {e}")
    })
}

/// Decode the JWT payload's `exp` claim (seconds); `None` when undecodable.
pub fn jwt_expiry(jwt: &str) -> Option<i64> {
    let part = jwt.split('.').nth(1)?;
    let part = match part.len() % 4 {
        2 => format!("{part}=="),
        3 => format!("{part}="),
        _ => part.to_owned(),
    };
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(part.as_bytes())
        .ok()?;
    let payload: Value = serde_json::from_slice(&bytes).ok()?;
    let exp = payload.get("exp")?.as_i64()?;
    (exp > 0).then_some(exp)
}

/// Second/ms epoch or numeric-string timestamps → epoch seconds.
pub fn norm_ts(value: Option<&Value>) -> Option<i64> {
    let raw = match value {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    if raw <= 0.0 {
        return None;
    }
    let secs = if raw >= 1e12 { raw / 1000.0 } else { raw };
    Some(secs.round() as i64)
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_nested() -> Value {
        json!({
            "auth": {
                "accessToken": "eyJhbGciOiJIUzUxMiJ9.eyJleHAiOjE5MDAwMDAwMDB9.sig",
                "refreshToken": "rt-1",
                "expiresAt": 1_900_000_000i64,
                "uuid": "u-u-i-d",
                "firstKeyfrom": "1700000000000",
                "latestKeyfrom": "1700000000001"
            },
            "account": {"uid": "u-123", "userId": "yid-1", "nickname": "nick"}
        })
    }

    fn sample_flat() -> Value {
        json!({
            "accessToken": "tok",
            "refreshToken": "rt",
            "uid": "u-flat",
            "nickname": "flat-nick"
        })
    }

    #[test]
    fn parse_nested_credential() {
        let data = parse_credential(sample_nested()).unwrap();
        assert_eq!(data.uid, "u-123");
        assert_eq!(data.user_id, "yid-1");
        assert_eq!(data.nickname, "nick");
        assert_eq!(data.uuid, "u-u-i-d");
        assert_eq!(data.expires_at_secs, 1_900_000_000);
        assert!(!data.access_token.expose_secret().is_empty());
        assert_eq!(data.refresh_token.expose_secret(), "rt-1");
    }

    #[test]
    fn parse_flat_credential() {
        let data = parse_credential(sample_flat()).unwrap();
        assert_eq!(data.uid, "u-flat");
        assert_eq!(data.nickname, "flat-nick");
        assert_eq!(data.access_token.expose_secret(), "tok");
        assert_eq!(data.expires_at_secs, 0, "no expiry recorded");
    }

    #[test]
    fn parse_rejects_missing_token() {
        let mut value = sample_nested();
        value["auth"]["accessToken"] = json!("");
        assert!(matches!(
            parse_credential(value),
            Err(CredentialError::MissingAccessToken)
        ));
        assert!(matches!(
            parse_credential(json!([])),
            Err(CredentialError::NotAnObject)
        ));
    }

    #[test]
    fn jwt_expiry_decodes_exp_claim() {
        // {"exp":1900000000} → base64url eyJleHAiOjE5MDAwMDAwMDB9
        let token = "h.eyJleHAiOjE5MDAwMDAwMDB9.s";
        assert_eq!(jwt_expiry(token), Some(1_900_000_000));
        assert_eq!(jwt_expiry("garbage"), None);
        assert_eq!(jwt_expiry("h.###.s"), None);
    }

    #[test]
    fn norm_ts_accepts_seconds_millis_and_strings() {
        assert_eq!(norm_ts(Some(&json!(1_900_000_000))), Some(1_900_000_000));
        assert_eq!(
            norm_ts(Some(&json!(1_900_000_000_000i64))),
            Some(1_900_000_000)
        );
        assert_eq!(norm_ts(Some(&json!("1900000000"))), Some(1_900_000_000));
        assert_eq!(norm_ts(Some(&json!(0))), None);
        assert_eq!(norm_ts(None), None);
    }

    #[test]
    fn safe_name_is_opaque_and_stable() {
        let a = safe_name(b"secret", "u-123");
        let b = safe_name(b"secret", "u-123");
        let c = safe_name(b"secret", "u-456");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("acct-"));
        assert!(!a.contains("u-123"));
    }

    fn tempdir(label: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("lap-cred-{label}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let _ = std::fs::create_dir_all(&path);
        path
    }

    #[test]
    fn atomic_write_leaves_no_temp_file_behind() {
        let dir = tempdir("atomic");
        let target = dir.join("lobsterai-u-1.json");
        atomic_write(&target, &sample_nested()).unwrap();
        assert!(target.is_file());
        let parsed = read_credential_file(&target).unwrap();
        assert_eq!(parsed.uid, "u-123");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(TEMP_FILE_PREFIX)
            })
            .collect();
        assert!(leftovers.is_empty(), "temp file survived the rename");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_refresh_persists_and_preserves_account() {
        let dir = tempdir("refresh");
        let target = dir.join("lobsterai-u-123.json");
        atomic_write(&target, &sample_nested()).unwrap();
        let credential = Credential::new(
            target.clone(),
            parse_credential(sample_nested()).unwrap(),
            b"s",
        );
        credential
            .apply_refresh("new-access", "new-refresh", 1_900_000_100)
            .unwrap();
        let data = read_credential_file(&target).unwrap();
        assert_eq!(data.access_token.expose_secret(), "new-access");
        assert_eq!(data.refresh_token.expose_secret(), "new-refresh");
        assert_eq!(data.expires_at_secs, 1_900_000_100);
        assert_eq!(data.nickname, "nick", "account section preserved");
        let raw: Value = serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
        assert_eq!(raw["auth"]["uuid"], "u-u-i-d", "keyfrom identity kept");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
