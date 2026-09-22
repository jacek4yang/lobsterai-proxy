//! Session identity: extraction, HMAC fingerprinting, and stable upstream
//! conversation IDs. Raw session identifiers are never logged or forwarded.

use axum::http::HeaderMap;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::fmt;

type HmacSha256 = Hmac<Sha256>;

/// Client-controlled session hints are deliberately small. The accepted value
/// is HMACed immediately, but bounding it also keeps request processing costs
/// predictable and rejects accidental payloads in affinity headers.
pub const MAX_CLIENT_SESSION_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSource {
    AnthropicMetadataUser,
    AnthropicMetadataSession,
    SessionAffinityHeader,
    SessionIdHeader,
}

impl fmt::Display for SessionSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AnthropicMetadataUser => "anthropic_metadata_user",
            Self::AnthropicMetadataSession => "anthropic_metadata_session",
            Self::SessionAffinityHeader => "session_affinity_header",
            Self::SessionIdHeader => "session_id_header",
        })
    }
}

/// A session identity safe for internal state and logs. The raw client value
/// never leaves the resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSession {
    pub fingerprint: String,
    pub source: SessionSource,
}

fn valid_session_hint(value: &str) -> Option<&str> {
    (!value.is_empty() && value.len() <= MAX_CLIENT_SESSION_BYTES).then_some(value)
}

fn metadata_session<'a>(request: &'a Value, field: &str) -> Option<&'a str> {
    request
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get(field))
        .and_then(Value::as_str)
        .and_then(valid_session_hint)
}

fn header_session<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(valid_session_hint)
}

/// Resolve an Anthropic frontend session in deterministic priority order:
///
/// 1. `metadata.user_id` (existing Claude Code behavior);
/// 2. `metadata.session_id`;
/// 3. `x-session-affinity` (Pi's standard affinity format);
/// 4. `x-session-id` (Pi's OpenRouter affinity format).
///
/// All Anthropic sources intentionally share the existing session fingerprint
/// domain. A client may change between the two Pi header formats without
/// losing continuity, while the Responses frontend remains separately
/// domain-separated by its caller. These headers are opaque affinity hints;
/// they never authorize requests or select an account directly.
pub fn resolve_anthropic_session(
    secret: &[u8],
    headers: &HeaderMap,
    request: &Value,
) -> Option<ResolvedSession> {
    let (raw, source) = metadata_session(request, "user_id")
        .map(|raw| (raw, SessionSource::AnthropicMetadataUser))
        .or_else(|| {
            metadata_session(request, "session_id")
                .map(|raw| (raw, SessionSource::AnthropicMetadataSession))
        })
        .or_else(|| {
            header_session(headers, "x-session-affinity")
                .map(|raw| (raw, SessionSource::SessionAffinityHeader))
        })
        .or_else(|| {
            header_session(headers, "x-session-id").map(|raw| (raw, SessionSource::SessionIdHeader))
        })?;
    Some(ResolvedSession {
        fingerprint: fingerprint(secret, raw),
        source,
    })
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
    use axum::http::HeaderValue;
    use serde_json::json;

    #[test]
    fn claude_metadata_precedence_and_fingerprint_stay_stable() {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-affinity", HeaderValue::from_static("pi-session"));
        headers.insert("x-session-id", HeaderValue::from_static("pi-openrouter"));
        let request = json!({
            "metadata": {"user_id": "user_x_session_a", "session_id": "sid_b"}
        });
        let resolved = resolve_anthropic_session(b"secret", &headers, &request).unwrap();
        assert_eq!(resolved.source, SessionSource::AnthropicMetadataUser);
        assert_eq!(
            resolved.fingerprint,
            fingerprint(b"secret", "user_x_session_a")
        );

        let resolved = resolve_anthropic_session(
            b"secret",
            &headers,
            &json!({"metadata": {"session_id": "sid_b"}}),
        )
        .unwrap();
        assert_eq!(resolved.source, SessionSource::AnthropicMetadataSession);
        assert_eq!(resolved.fingerprint, fingerprint(b"secret", "sid_b"));
    }

    #[test]
    fn pi_affinity_headers_are_stable_and_have_deterministic_precedence() {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-affinity", HeaderValue::from_static("pi-session"));
        headers.insert("x-session-id", HeaderValue::from_static("pi-openrouter"));
        let first = resolve_anthropic_session(b"secret", &headers, &json!({})).unwrap();
        let second = resolve_anthropic_session(b"secret", &headers, &json!({})).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.source, SessionSource::SessionAffinityHeader);
        assert_eq!(first.fingerprint, fingerprint(b"secret", "pi-session"));

        headers.remove("x-session-affinity");
        let fallback = resolve_anthropic_session(b"secret", &headers, &json!({})).unwrap();
        assert_eq!(fallback.source, SessionSource::SessionIdHeader);
        assert_eq!(
            fallback.fingerprint,
            fingerprint(b"secret", "pi-openrouter")
        );

        headers.insert("x-session-id", HeaderValue::from_static("pi-session"));
        let same_logical_session =
            resolve_anthropic_session(b"secret", &headers, &json!({})).unwrap();
        assert_eq!(same_logical_session.fingerprint, first.fingerprint);
    }

    #[test]
    fn installed_pi_default_shape_has_no_invented_session() {
        // Pi 0.87.0 omits metadata and affinity headers for custom Anthropic
        // providers unless its sendSessionAffinityHeaders compatibility option
        // is enabled. Standard SDK headers must not become a global fallback.
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", HeaderValue::from_static("pi/0.87.0"));
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        assert_eq!(
            resolve_anthropic_session(b"secret", &headers, &json!({})),
            None
        );
    }

    #[test]
    fn invalid_or_oversized_sources_are_ignored_without_aliasing() {
        let oversized = "x".repeat(MAX_CLIENT_SESSION_BYTES + 1);
        let request = json!({"metadata": {"user_id": oversized, "session_id": "valid"}});
        let resolved = resolve_anthropic_session(b"secret", &HeaderMap::new(), &request).unwrap();
        assert_eq!(resolved.source, SessionSource::AnthropicMetadataSession);

        let mut headers = HeaderMap::new();
        headers.insert("x-session-affinity", HeaderValue::from_static(""));
        headers.insert(
            "x-session-id",
            HeaderValue::from_bytes(&[0xff, 0xfe]).expect("opaque header value"),
        );
        assert_eq!(
            resolve_anthropic_session(b"secret", &headers, &json!({})),
            None
        );
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
