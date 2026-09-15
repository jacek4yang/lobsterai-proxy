//! Privacy-safe sanitization for error text, logs, and debug output.
//!
//! Every message that reaches a client or a log line passes through here.
//! Access tokens, refresh tokens, bearer credentials, JWTs, cookies, gateway
//! API keys, and raw session identifiers must never appear in output.

use serde_json::Value;

const MAX_PUBLIC_TEXT_CHARS: usize = 1024;

/// Replace exact secret literals, then scrub credential-shaped words.
pub fn sanitize_text(input: &str, exact_secrets: &[&str]) -> String {
    let mut output = input.to_owned();
    for secret in exact_secrets {
        if !secret.is_empty() {
            output = output.replace(secret, "[REDACTED]");
        }
    }
    let mut sanitized = Vec::with_capacity(16);
    let mut redact_next = false;
    for word in output.split_whitespace() {
        let trimmed = word.trim_matches(|c: char| {
            matches!(c, ',' | ';' | ':' | '"' | '\'' | '(' | ')' | '[' | ']')
        });
        let lower = trimmed.to_ascii_lowercase();
        let sensitive_assignment = [
            "authorization=",
            "authorization:",
            "x-api-key=",
            "x-api-key:",
            "api_key=",
            "api-key=",
            "access_token=",
            "refresh_token=",
            "refreshtoken=",
            "accesstoken=",
            "x-refresh-token:",
            "cookie=",
        ]
        .iter()
        .any(|prefix| lower.starts_with(prefix));
        let sensitive_value = redact_next
            || sensitive_assignment
            || lower.starts_with("sk-")
            || lower.starts_with("bearer")
            || looks_like_jwt(trimmed);
        if sensitive_value {
            sanitized.push("[REDACTED]".to_owned());
        } else {
            sanitized.push(word.to_owned());
        }
        redact_next = lower == "bearer" || lower == "authorization:" || lower == "x-api-key:";
    }
    truncate_chars(&sanitized.join(" "), MAX_PUBLIC_TEXT_CHARS)
}

/// Recursively redact credential-shaped JSON fields and scrub string values.
pub fn sanitize_json(mut value: Value, exact_secrets: &[&str]) -> Value {
    redact_value(&mut value, exact_secrets);
    value
}

fn redact_value(value: &mut Value, exact_secrets: &[&str]) {
    match value {
        Value::Object(object) => {
            for (name, value) in object {
                if sensitive_name(name) {
                    *value = Value::String("[REDACTED]".into());
                } else {
                    redact_value(value, exact_secrets);
                }
            }
        }
        Value::Array(array) => {
            for value in array {
                redact_value(value, exact_secrets);
            }
        }
        Value::String(text) => *text = sanitize_text(text, exact_secrets),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn sensitive_name(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase();
    let normalized = normalized.replace(['-', '_'], "");
    normalized == "authorization"
        || normalized == "xapikey"
        || normalized == "apikey"
        || normalized == "accesstoken"
        || normalized == "refreshtoken"
        || normalized == "xrefreshtoken"
        || normalized == "cookie"
        || normalized == "setcookie"
        || normalized == "idtoken"
        || normalized == "sessionstate"
        || normalized.ends_with("secret")
}

fn looks_like_jwt(value: &str) -> bool {
    let mut segments = value.split('.');
    matches!(
        (segments.next(), segments.next(), segments.next(), segments.next()),
        (Some(a), Some(b), Some(c), None)
            if a.len() >= 8 && b.len() >= 8 && c.len() >= 2
                && [a, b, c].iter().all(|segment| {
                    segment.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                })
                && c.len() >= 2
    )
}

fn truncate_chars(input: &str, max: usize) -> String {
    let mut chars = input.chars();
    let prefix = chars.by_ref().take(max).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recursive_redaction_removes_credentials() {
        let value = sanitize_json(
            json!({
                "authorization": "Bearer secret-token",
                "nested": {"api_key": "secret-key", "message": "Bearer another-secret"},
                "auth": {"refreshToken": "rt-value"},
                "safe": "hello"
            }),
            &["secret-key"],
        );
        let rendered = value.to_string();
        assert!(!rendered.contains("secret-token"));
        assert!(!rendered.contains("another-secret"));
        assert!(!rendered.contains("secret-key"));
        assert!(!rendered.contains("rt-value"));
        assert!(rendered.contains("hello"));
    }

    #[test]
    fn jwt_and_bearer_words_are_redacted() {
        let output = sanitize_text(
            "failed with eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig and Bearer abc123",
            &[],
        );
        assert!(!output.contains("eyJ"));
        assert!(!output.contains("abc123"));
    }

    #[test]
    fn exact_secrets_are_removed() {
        let output = sanitize_text("gateway-secret and sk-provider-secret", &["gateway-secret"]);
        assert!(!output.contains("secret"));
        assert_eq!(output.matches("[REDACTED]").count(), 2);
    }

    #[test]
    fn long_text_is_truncated() {
        let output = sanitize_text(&"x".repeat(2000), &[]);
        assert!(output.chars().count() <= MAX_PUBLIC_TEXT_CHARS + 1);
    }
}
