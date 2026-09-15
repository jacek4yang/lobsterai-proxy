//! LobsterAI upstream: shared HTTP client construction and the generation
//! call with strict retry invariants.
//!
//! One logical generation = at most one successful generation:
//! - HTTP 401 before any output → single-flight refresh, retry the SAME
//!   account exactly once; a second 401 stops.
//! - HTTP 429 before any output → (account, model) cooldown, failover to
//!   another account.
//! - Out-of-credits (business code / quota error) → long cooldown, failover.
//! - HTTP 403 → stop (no automatic account switching to bypass policy).
//! - 5xx / transport errors → stop immediately, no replay, no failover.

use bytes::Bytes;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::credential::Credential;
use super::headers;
use super::pool::Pool;
use crate::config::{Config, UpstreamConfig};
use crate::error;

/// The one chat endpoint this proxy talks to (OpenAI-compatible, stream-only).
pub const CHAT_COMPLETIONS_PATH: &str = "/api/proxy/v1/chat/completions";

/// Fields forwarded to the backend when present on the converted body.
const PASSTHROUGH_BODY_KEYS: [&str; 14] = [
    "model",
    "messages",
    "tools",
    "tool_choice",
    "temperature",
    "top_p",
    "top_k",
    "max_tokens",
    "stop",
    "presence_penalty",
    "frequency_penalty",
    "reasoning_effort",
    "prompt_cache_key",
    "user",
];

/// Build the long-lived shared client. No proxy configured → deterministic
/// direct connection (environment proxy variables are ignored on purpose).
pub fn build_client(config: &UpstreamConfig) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .user_agent(super::credential::USER_AGENT)
        .connect_timeout(Duration::from_secs(config.connect_timeout_secs.max(1)))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(16)
        .tcp_nodelay(true)
        .http2_adaptive_window(true);
    match &config.proxy {
        Some(proxy_url) => {
            let proxy = reqwest::Proxy::all(proxy_url).expect("configured proxy URL must parse");
            builder = builder.proxy(proxy);
        }
        None => {
            builder = builder.no_proxy();
        }
    }
    builder.build().expect("shared HTTP client must build")
}

/// Failure surfaced by the generation call (already sanitized).
#[derive(Debug, Clone)]
pub struct ApiFailure {
    pub status: u16,
    pub kind: &'static str,
    pub message: String,
    pub retry_after_secs: Option<i64>,
    /// True when the failure occurred before any semantic output (retry-safe).
    pub before_output: bool,
}

pub enum GenerateOutcome {
    /// Upstream accepted the request; the SSE byte stream is ready.
    Started(Box<StartedGeneration>),
    Failed(ApiFailure),
}

pub struct StartedGeneration {
    pub model: String,
    pub credential: Arc<Credential>,
    pub response: reqwest::Response,
    pub started_at: Instant,
    /// Set when a failover occurred during this logical generation.
    pub failover_count: u32,
    pub refresh_retry: bool,
}

/// Send the converted OpenAI chat body to LobsterAI, applying the retry
/// invariants. `body` must already contain the final `model` field.
pub async fn generate(
    pool: &Pool,
    http: &reqwest::Client,
    config: &Config,
    session_fp: Option<&str>,
    body: &Value,
    metrics: &crate::observability::Metrics,
) -> GenerateOutcome {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&config.model.default)
        .to_owned();
    let upstream_body = filter_body(body, &model);
    let url = format!("{}{CHAT_COMPLETIONS_PATH}", config.upstream.base_url);
    let started_at = Instant::now();
    let mut failover_count = 0u32;
    let mut refresh_retry_done = false;
    let mut retried_401_account: Option<String> = None;

    loop {
        // 401 retries reuse the SAME account, bypassing sticky re-pick.
        let credential = match &retried_401_account {
            Some(uid) => pool
                .credential_by_safe_name(uid)
                .or_else(|| pool.pick(session_fp, &model)),
            None => pool.pick(session_fp, &model),
        };
        let Some(credential) = credential else {
            // No healthy account for this model: fast-fail locally.
            if let Some(until) = pool.all_cooled_until(&model) {
                let retry_after = (until - super::credential::now_secs()).max(1);
                return GenerateOutcome::Failed(ApiFailure {
                    status: 429,
                    kind: error::RATE_LIMIT,
                    message: format!(
                        "model {model} quota is cooling down on all accounts; retry after {retry_after}s"
                    ),
                    retry_after_secs: Some(retry_after),
                    before_output: true,
                });
            }
            return GenerateOutcome::Failed(ApiFailure {
                status: 503,
                kind: error::API,
                message:
                    "no healthy account available (not logged in, file missing, or all cooling)"
                        .into(),
                retry_after_secs: None,
                before_output: true,
            });
        };

        let mut request_headers = headers::chat_headers(&credential);

        let send_result = http
            .post(&url)
            .headers(request_headers)
            .json(&upstream_body)
            .send()
            .await;
        request_headers = headers::chat_headers(&credential);
        let _ = request_headers;
        let response = match send_result {
            Ok(response) => response,
            Err(err) => {
                // Transport error: never replay, never failover.
                metrics.record_transport_error();
                pool.note_error_threshold(&credential);
                return GenerateOutcome::Failed(ApiFailure {
                    status: 502,
                    kind: error::API,
                    message: crate::redaction::sanitize_text(
                        &format!("upstream connection failed: {err}"),
                        &[credential.access_token().as_str()],
                    ),
                    retry_after_secs: None,
                    before_output: true,
                });
            }
        };
        let status = response.status().as_u16();
        if status == 200 {
            // Request-level success is recorded once, at completion, by the
            // caller; recording here would double-count.
            return GenerateOutcome::Started(Box::new(StartedGeneration {
                model,
                credential,
                response,
                started_at,
                failover_count,
                refresh_retry: refresh_retry_done,
            }));
        }

        // Error path: read the body for cooldown parsing / sanitization.
        let raw = response.bytes().await.unwrap_or_else(|_| Bytes::new());
        // Out-of-credit detection: the backend reports exhausted quota in the
        // body (HTTP 200-with-error does not occur; it surfaces as 4xx/429).
        if is_out_of_credits(status, &raw) {
            pool.note_hard_credit(&credential);
            failover_count += 1;
            if failover_count > (pool.len() as u32).max(1) {
                return failover_exhausted(&model, pool);
            }
            continue;
        }
        match status {
            401 => {
                metrics.record_upstream_401();
                if retried_401_account.is_none() {
                    // First 401: refresh the SAME account, retry once.
                    tracing::warn!(credential = %credential.safe_name, status = 401, "upstream rejected token; refreshing");
                    let refresh_result =
                        super::refresh::force_refresh(&credential, http, &config.upstream.base_url)
                            .await;
                    match refresh_result {
                        Ok(()) => {
                            metrics.record_refresh(true);
                            retried_401_account = Some(credential.safe_name.clone());
                            refresh_retry_done = true;
                            continue;
                        }
                        Err(err) => {
                            metrics.record_refresh(false);
                            pool.note_status(&credential, 401, &model, &raw);
                            return GenerateOutcome::Failed(ApiFailure {
                                status: 401,
                                kind: error::AUTHENTICATION,
                                message: err.message,
                                retry_after_secs: None,
                                before_output: true,
                            });
                        }
                    }
                }
                // Second 401: stop.
                pool.note_status(&credential, 401, &model, &raw);
                return GenerateOutcome::Failed(ApiFailure {
                    status: 401,
                    kind: error::AUTHENTICATION,
                    message: "LobsterAI rejected the refreshed token; please re-login (lobsterai-proxy login)".into(),
                    retry_after_secs: None,
                    before_output: true,
                });
            }
            429 => {
                metrics.record_upstream_429();
                pool.note_status(&credential, 429, &model, &raw);
                failover_count += 1;
                if failover_count > (pool.len() as u32).max(1) {
                    return failover_exhausted(&model, pool);
                }
                continue;
            }
            403 => {
                metrics.record_upstream_403();
                pool.note_status(&credential, 403, &model, &raw);
                return GenerateOutcome::Failed(ApiFailure {
                    status: 403,
                    kind: error::PERMISSION,
                    message: crate::redaction::sanitize_text(
                        &format!(
                            "LobsterAI returned HTTP 403 (permission or content policy): {}",
                            String::from_utf8_lossy(&raw)
                        ),
                        &[
                            credential.access_token().as_str(),
                            credential.refresh_token().as_str(),
                        ],
                    ),
                    retry_after_secs: None,
                    before_output: true,
                });
            }
            500..=599 => {
                metrics.record_upstream_5xx();
                return GenerateOutcome::Failed(ApiFailure {
                    status,
                    kind: error::API,
                    message: crate::redaction::sanitize_text(
                        &format!(
                            "upstream server error (HTTP {status}): {}",
                            String::from_utf8_lossy(&raw)
                        ),
                        &[
                            credential.access_token().as_str(),
                            credential.refresh_token().as_str(),
                        ],
                    ),
                    retry_after_secs: None,
                    before_output: true,
                });
            }
            _ => {
                pool.note_error_threshold(&credential);
                return GenerateOutcome::Failed(ApiFailure {
                    status,
                    kind: error::API,
                    message: crate::redaction::sanitize_text(
                        &format!(
                            "upstream rejected the request (HTTP {status}): {}",
                            String::from_utf8_lossy(&raw)
                        ),
                        &[
                            credential.access_token().as_str(),
                            credential.refresh_token().as_str(),
                        ],
                    ),
                    retry_after_secs: None,
                    before_output: true,
                });
            }
        }
    }
}

/// Heuristic out-of-credit classifier for the upstream error body. The
/// backend signals exhausted quota via 402/403-with-quota-text or the
/// business envelope `code` in the 4xx body; all detections are conservative
/// (a false negative just means the account cools on its natural path).
fn is_out_of_credits(status: u16, raw: &[u8]) -> bool {
    if !matches!(status, 402 | 403 | 429) {
        return false;
    }
    let text = String::from_utf8_lossy(raw).to_ascii_lowercase();
    [
        "insufficient",
        "not enough credit",
        "credit",
        "quota exceeded",
        "balance",
        "积分不足",
        "余额不足",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn failover_exhausted(model: &str, pool: &Pool) -> GenerateOutcome {
    let retry_after = pool
        .all_cooled_until(model)
        .map(|until| (until - super::credential::now_secs()).max(1));
    GenerateOutcome::Failed(ApiFailure {
        status: 429,
        kind: error::RATE_LIMIT,
        message: format!("all accounts are rate-limited for model {model}"),
        retry_after_secs: retry_after,
        before_output: true,
    })
}

/// Restrict the outgoing body to known-safe passthrough keys and force the
/// streaming protocol (the backend only supports streaming).
fn filter_body(body: &Value, model: &str) -> Value {
    let mut out = serde_json::Map::new();
    let object = body.as_object().cloned().unwrap_or_default();
    for key in PASSTHROUGH_BODY_KEYS {
        if let Some(value) = object.get(key) {
            out.insert(key.to_owned(), value.clone());
        }
    }
    out.insert("model".into(), Value::String(model.to_owned()));
    out.insert("stream".into(), Value::Bool(true));
    out.entry("stream_options".to_owned())
        .or_insert_with(|| serde_json::json!({"include_usage": true}));
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_body_forces_stream_and_usage() {
        let body = serde_json::json!({
            "model": "deepseek-flash",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 100,
            "metadata": {"should": "be dropped"},
            "thinking": {"type": "enabled"}
        });
        let filtered = filter_body(&body, "deepseek-flash");
        assert_eq!(filtered["stream"], true);
        assert_eq!(filtered["stream_options"]["include_usage"], true);
        assert_eq!(filtered["max_tokens"], 100);
        assert!(filtered.get("metadata").is_none());
        assert!(filtered.get("thinking").is_none());
    }

    #[test]
    fn passthrough_keeps_sampling_fields() {
        let body = serde_json::json!({
            "model": "m",
            "messages": [],
            "temperature": 0.7,
            "top_p": 0.9,
            "top_k": 40,
            "stop": ["END"]
        });
        let filtered = filter_body(&body, "m");
        assert_eq!(filtered["temperature"], 0.7);
        assert_eq!(filtered["top_p"], 0.9);
        assert_eq!(filtered["top_k"], 40);
        assert_eq!(filtered["stop"], serde_json::json!(["END"]));
    }

    #[test]
    fn out_of_credit_classifier_is_conservative() {
        assert!(is_out_of_credits(402, b"insufficient credits"));
        assert!(is_out_of_credits(
            429,
            "{\"code\":1,\"msg\":\"\u{79ef}\u{5206}\u{4e0d}\u{8db3}\"}".as_bytes()
        ));
        assert!(
            !is_out_of_credits(429, b"rate limited"),
            "plain 429 stays 429"
        );
        assert!(
            !is_out_of_credits(500, b"insufficient credits"),
            "5xx stops anyway"
        );
        assert!(!is_out_of_credits(401, b"credit"), "401 keeps refresh path");
    }
}
