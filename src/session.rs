//! Session identity: extraction, HMAC fingerprinting, and stable upstream
//! conversation IDs. Raw session identifiers are never logged or forwarded.

use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Raw session identity from an Anthropic request, in priority order:
/// 1. `metadata.user_id` — Claude Code formats it as `<user>_<account>_session_<id>`.
/// 2. `metadata.session_id` when present.
///
/// `None` when neither is present: without a stable identity there is no
/// session-scoped behavior (never guessed from IP, connection, or recency).
pub fn extract_raw_session(request: &Value) -> Option<&str> {
    let metadata = request.get("metadata")?.as_object()?;
    metadata
        .get("user_id")
        .and_then(Value::as_str)
        .or_else(|| metadata.get("session_id").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
}

/// HMAC-SHA256(server-secret, raw session id), rendered as 16 lowercase hex
/// characters. Stable across requests; safe for logs and telemetry.
pub fn fingerprint(secret: &[u8], raw_session_id: &str) -> String {
    fingerprint_domain(secret, b"lobsterai-proxy/session/v1", raw_session_id)
}

/// Domain-separated fingerprint: the same raw id fingerprints differently
/// per protocol frontend, so Anthropic and Responses identities can never
/// collide in the shadow store or credential stickiness.
pub fn fingerprint_domain(secret: &[u8], domain: &[u8], raw_id: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(domain);
    mac.update(b"\0");
    mac.update(raw_id.as_bytes());
    let digest = mac.finalize().into_bytes();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    hex[..16].to_owned()
}

/// Deterministic upstream `X-Conversation-ID` (UUID shape) derived from the
/// session identity. Falls back to a random UUID when no session exists.
pub fn conversation_id(secret: &[u8], session_fp: Option<&str>) -> String {
    let full: [u8; 32] = match session_fp {
        Some(fp) => {
            let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
            mac.update(b"lobsterai-proxy/conversation/v1\0");
            mac.update(fp.as_bytes());
            mac.finalize().into_bytes().into()
        }
        None => {
            use rand::Rng;
            let mut bytes = [0u8; 32];
            rand::rng().fill_bytes(&mut bytes);
            bytes
        }
    };
    format_uuid(full[..16].try_into().expect("16 bytes"))
}

/// RFC-4122-shaped rendering of 16 bytes (version/variant bits set for
/// determinism-independent wire shape stability).
fn format_uuid(bytes: [u8; 16]) -> String {
    let mut bytes = bytes;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Random 32-character lowercase-hex request id (X-Request-ID shape).
pub fn random_hex_32() -> String {
    let mut bytes = [0u8; 16];
    use rand::Rng;
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Random 16-character lowercase-hex span id.
pub fn random_hex_16() -> String {
    let mut bytes = [0u8; 8];
    use rand::Rng;
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn request_id() -> String {
    format!("req_{}", random_hex_16())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn raw_session_prefers_user_id_and_fails_safe() {
        let request = json!({"metadata": {"user_id": "user_x_session_a", "session_id": "sid_b"}});
        assert_eq!(extract_raw_session(&request), Some("user_x_session_a"));
        let request = json!({"metadata": {"session_id": "sid_b"}});
        assert_eq!(extract_raw_session(&request), Some("sid_b"));
        assert_eq!(
            extract_raw_session(&json!({"metadata": {"user_id": ""}})),
            None
        );
        assert_eq!(extract_raw_session(&json!({})), None);
    }

    #[test]
    fn fingerprint_is_stable_secret_bound_and_opaque() {
        let a = fingerprint(b"secret", "user_abc_session_9f8e7d");
        let b = fingerprint(b"secret", "user_abc_session_9f8e7d");
        let c = fingerprint(b"other", "user_abc_session_9f8e7d");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!a.contains("9f8e7d"));
    }

    #[test]
    fn conversation_id_is_uuid_shaped_and_deterministic() {
        let fp = fingerprint(b"secret", "session-a");
        let a = conversation_id(b"secret", Some(&fp));
        let b = conversation_id(b"secret", Some(&fp));
        assert_eq!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(a.matches('-').count(), 4);
        assert_ne!(
            a,
            conversation_id(b"secret", Some(&fingerprint(b"secret", "session-b")))
        );
        assert_ne!(a, conversation_id(b"secret", None), "random fallback");
    }
}
