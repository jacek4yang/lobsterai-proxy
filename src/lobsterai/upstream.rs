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
    /// The upstream SSE byte stream, positioned after `prefix`.
    pub stream: ByteStream,
    /// Bytes consumed while probing the stream head; MUST be emitted before
    /// `stream` so the client sees byte-identical output.
    pub prefix: Vec<u8>,
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
            // The backend reports quota exhaustion as HTTP 200 whose body is
            // an immediate `event:error` frame (code 40201). The status code
            // alone therefore cannot decide success: peek at the stream head
            // so an exhausted account cools down and fails over instead of
            // being pinned by sticky sessions on every subsequent request.
            match peek_stream_error(response, &credential).await {
                Peek::Healthy { prefix, stream } => {
                    // Request-level success is recorded once, at completion,
                    // by the caller; recording here would double-count.
                    return GenerateOutcome::Started(Box::new(StartedGeneration {
                        model,
                        credential,
                        stream,
                        prefix,
                        started_at,
                        failover_count,
                        refresh_retry: refresh_retry_done,
                    }));
                }
                Peek::Errored { error } => {
                    metrics.record_upstream_200_error();
                    tracing::warn!(
                        credential = %credential.safe_name,
                        code = error.code,
                        quota_exhausted = error.is_quota_exhausted(),
                        "upstream returned an in-stream error over HTTP 200"
                    );
                    if error.is_quota_exhausted() {
                        // Learn the real balance (0) so selection/status stop
                        // treating this account as a candidate, then rotate.
                        pool.set_credits(&credential.uid(), 0);
                        pool.note_hard_credit(&credential);
                        failover_count += 1;
                        if failover_count > (pool.len() as u32).max(1) {
                            return failover_exhausted(&model, pool);
                        }
                        continue;
                    }
                    // Any other in-stream error is terminal (no replay).
                    pool.note_error_threshold(&credential);
                    return GenerateOutcome::Failed(ApiFailure {
                        status: 502,
                        kind: error::API,
                        message: crate::redaction::sanitize_text(
                            &format!("upstream returned an error event: {}", error.message),
                            &[
                                credential.access_token().as_str(),
                                credential.refresh_token().as_str(),
                            ],
                        ),
                        retry_after_secs: None,
                        before_output: true,
                    });
                }
                Peek::Unavailable => {
                    // The peek itself failed (transport error mid-prefix):
                    // treat like a transport error — never replay.
                    metrics.record_transport_error();
                    pool.note_error_threshold(&credential);
                    return GenerateOutcome::Failed(ApiFailure {
                        status: 502,
                        kind: error::API,
                        message: "upstream stream failed before any output".into(),
                        retry_after_secs: None,
                        before_output: true,
                    });
                }
            }
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

/// Business error code the backend returns inside an `event:error` SSE frame
/// when the account's quota is exhausted. The HTTP status is **200**; the
/// failure is only visible in the stream body.
pub const QUOTA_EXHAUSTED_CODE: i64 = 40201;

/// A stream-body error the backend reports after an HTTP 200. Surfaced as an
/// early failure so the retry invariants can run (cooldown + failover) even
/// though the transport-level status was a success.
#[derive(Debug, Clone)]
pub struct StreamError {
    /// Business code from the error envelope, when present.
    pub code: Option<i64>,
    /// Sanitized message (never carries credential material).
    pub message: String,
}

impl StreamError {
    /// True when this is the quota-exhausted signal (long cooldown + rotate).
    pub fn is_quota_exhausted(&self) -> bool {
        self.code == Some(QUOTA_EXHAUSTED_CODE)
    }
}

/// Parse an `event:error` SSE payload into a [`StreamError`].
///
/// The backend emits frames shaped
/// `event:error\ndata:{"type":"error","error":{"type":"proxy_error",
/// "message":"…","code":40201}}`. API-style error envelopes
/// (`{"error": {"message", "code"}}`) are also accepted, so a shape change
/// degrades to "recognized error" rather than "silently ignored".
fn parse_stream_error(payload: &Value) -> Option<StreamError> {
    let error = payload.get("error")?;
    let code = error.get("code").and_then(Value::as_i64);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("upstream error");
    Some(StreamError {
        code,
        message: message.to_owned(),
    })
}

/// Defensive bound on the prefix bytes inspected while looking for an early
/// stream error. The backend's quota frame is ~131 bytes; this cap keeps a
/// pathological upstream from making the proxy buffer without limit before
/// the first semantic event.
const MAX_PREFIX_INSPECT_BYTES: usize = 8 * 1024;

/// Scan a byte prefix for a *complete* early error frame.
///
/// Returns `Some((error, scanned_len))` when a complete `event:error` frame
/// was found within `prefix` — `scanned_len` is the byte offset just past the
/// frame, so the caller can strip it. `None` means "no decision yet": the
/// prefix held no complete error frame (callers may accumulate more bytes).
///
/// Only frames that arrive **before** any semantic SSE data are considered:
/// an error after real content is a mid-stream failure and must not be
/// replayed (the retry invariants forbid it).
fn scan_prefix_for_stream_error(prefix: &[u8]) -> Option<(StreamError, usize)> {
    let text = String::from_utf8_lossy(prefix);
    let mut offset = 0usize;
    let mut saw_data = false;
    for block in text.split("\n\n") {
        // `split` yields the trailing remainder as a final block; it is not
        // terminated by a blank line, so it is still incomplete and must not
        // be judged. Skipping it is what keeps a partial frame "undecided".
        if offset + block.len() >= text.len() {
            break;
        }
        // The consumed length covers this block plus the blank-line separator.
        let next_offset = offset + block.len() + 2;
        let mut is_error = false;
        let mut payload: Option<&str> = None;
        for line in block.lines() {
            if let Some(event) = line.strip_prefix("event:") {
                if event.trim() == "error" {
                    is_error = true;
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim();
                if data == "[DONE]" {
                    continue;
                }
                if is_error {
                    payload = Some(data);
                } else {
                    // Any non-error data frame is semantic upstream output.
                    saw_data = true;
                }
            }
        }
        if is_error {
            if let Some(payload) = payload {
                if let Ok(parsed) = serde_json::from_str::<Value>(payload) {
                    if let Some(error) = parse_stream_error(&parsed) {
                        return Some((error, next_offset));
                    }
                }
            }
            // An error frame with an unparseable payload still means "not a
            // usable stream": report it so the caller fails over.
            return Some((
                StreamError {
                    code: None,
                    message: "upstream returned an error event".into(),
                },
                next_offset,
            ));
        }
        if saw_data {
            // Semantic output already started: never treat a later frame as
            // a pre-output failure.
            return None;
        }
        offset = next_offset;
    }
    None
}

/// Result of peeking at the head of an HTTP-200 upstream stream.
enum Peek {
    /// No early error frame. `prefix` holds the already-consumed bytes that
    /// the caller must replay ahead of the remaining body (byte-identical).
    Healthy { prefix: Vec<u8>, stream: ByteStream },
    /// A complete early `event:error` frame was found before any semantic
    /// output.
    Errored { error: StreamError },
    /// Reading the prefix failed (transport error), or the stream ended
    /// before any decision could be made.
    Unavailable,
}

/// The upstream SSE byte stream (boxed so a partially-consumed stream can be
/// handed back to the caller alongside its consumed prefix).
pub type ByteStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<Bytes>> + Send>>;

impl StartedGeneration {
    /// The upstream byte stream with the probing prefix replayed first, so
    /// downstream consumers see byte-identical output regardless of peeking.
    pub fn into_byte_stream(self) -> ByteStream {
        let Self { prefix, stream, .. } = self;
        if prefix.is_empty() {
            return stream;
        }
        let head = futures_util::stream::once(async move { Ok(Bytes::from(prefix)) });
        Box::pin(futures_util::StreamExt::chain(head, stream))
    }
}

/// Probe the head of a 200 response for an early `event:error` frame.
///
/// The backend answers an exhausted account with `HTTP 200` and a body whose
/// first bytes are `event:error\ndata:{…"code":40201…}`. Detecting that here
/// lets the retry invariants cool down + fail over while nothing has been
/// streamed to the client yet.
///
/// The stream is **never truncated**: bytes consumed while probing are
/// returned in `prefix` so the caller can replay them verbatim.
async fn peek_stream_error(response: reqwest::Response, _credential: &Arc<Credential>) -> Peek {
    use futures_util::StreamExt as _;

    let mut stream: ByteStream = Box::pin(response.bytes_stream());
    let mut prefix: Vec<u8> = Vec::with_capacity(512);
    loop {
        // Only a *complete* frame can be judged; the backend splits the error
        // frame across several chunks.
        if let Some((error, _consumed)) = scan_prefix_for_stream_error(&prefix) {
            return Peek::Errored { error };
        }
        // A *complete* frame boundary (`\n\n`) with no error frame before it
        // means real data has started: the head is healthy.
        if prefix.windows(2).any(|w| w == b"\n\n") {
            return Peek::Healthy { prefix, stream };
        }
        if prefix.len() >= MAX_PREFIX_INSPECT_BYTES {
            // Inconclusive but oversized: do not block the request further.
            return Peek::Healthy { prefix, stream };
        }
        match stream.next().await {
            Some(Ok(bytes)) => prefix.extend_from_slice(&bytes),
            // EOF/transport failure with no complete frame and no data.
            Some(Err(_)) | None => return Peek::Unavailable,
        }
    }
}

/// Heuristic out-of-credit classifier for the upstream error body. The
/// backend signals exhausted quota via the `event:error` stream frame (code
/// 40201) or a 402/403/429 body carrying quota text; all detections are
/// conservative (a false negative just means the account cools on its
/// natural path).
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

    #[test]
    fn stream_error_prefix_detects_quota_exhaustion() {
        // The exact frame the backend emits for an exhausted account
        // (transcribed from a live probe; HTTP 200 + this body).
        let raw = b"event:error\ndata:{\"type\":\"error\",\"error\":{\"type\":\"proxy_error\",\
\"message\":\"\xe5\x85\x8d\xe8\xb4\xb9\xe9\xa2\x9d\xe5\xba\xa6\xe5\xb7\xb2\xe7\x94\xa8\xe5\xae\x8c\
\xef\xbc\x8c\xe8\xaf\xb7\xe5\x8d\x87\xe7\xba\xa7\xe5\xa5\x97\xe9\xa4\x90\",\"code\":40201}}\n\n";
        let (error, consumed) = scan_prefix_for_stream_error(raw).expect("quota frame detected");
        assert!(error.is_quota_exhausted(), "code 40201 is quota exhausted");
        assert_eq!(error.code, Some(QUOTA_EXHAUSTED_CODE));
        assert_eq!(consumed, raw.len(), "consumes exactly the error frame");
    }

    #[test]
    fn stream_error_prefix_ignores_normal_chunks() {
        // A healthy OpenAI chunk must never be mistaken for a failure.
        let raw = br#"data:{"id":"x","choices":[{"index":0,"delta":{"role":"assistant"}}]}"#;
        assert!(scan_prefix_for_stream_error(raw).is_none());
        // Partial error frame (no blank-line terminator yet): undecided, so
        // the caller keeps accumulating instead of failing prematurely.
        let partial = b"event:error\ndata:{\"type\":\"error\",\"error\":{\"code\":40201}}";
        assert!(scan_prefix_for_stream_error(partial).is_none());
    }

    #[test]
    fn stream_error_prefix_is_safe_at_every_byte_boundary() {
        let raw = b"event:error\ndata:{\"type\":\"error\",\"error\":{\"message\":\"quota\",\"code\":40201}}\n\n";
        for split in 1..raw.len() {
            assert!(
                scan_prefix_for_stream_error(&raw[..split]).is_none(),
                "incomplete prefix at byte {split} must remain undecided"
            );
            let mut accumulated = raw[..split].to_vec();
            accumulated.extend_from_slice(&raw[split..]);
            let (error, consumed) = scan_prefix_for_stream_error(&accumulated)
                .expect("complete fragmented frame is detected");
            assert!(error.is_quota_exhausted());
            assert_eq!(consumed, raw.len());
        }
    }

    #[test]
    fn stream_error_after_semantic_data_is_not_a_pre_output_failure() {
        // Once real content has streamed, a later error frame must not be
        // treated as retry-safe (the no-replay invariant).
        let raw = br#"data:{"choices":[{"index":0,"delta":{"content":"hi"}}]}

event:error
data:{"type":"error","error":{"code":40201}}

"#;
        assert!(
            scan_prefix_for_stream_error(raw).is_none(),
            "semantic output already started"
        );
    }

    #[test]
    fn unknown_error_code_is_reported_but_not_quota() {
        let raw = b"event:error\ndata:{\"type\":\"error\",\"error\":{\"message\":\"boom\",\"code\":50001}}\n\n";
        let (error, _) = scan_prefix_for_stream_error(raw).expect("frame detected");
        assert!(!error.is_quota_exhausted(), "only 40201 rotates accounts");
        assert_eq!(error.message, "boom");
    }

    #[test]
    fn started_generation_replays_prefix_before_stream() {
        use futures_util::StreamExt as _;
        let dir = std::env::temp_dir().join(format!("lap-upstream-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let data = super::super::credential::parse_credential(serde_json::json!({
            "auth": {"accessToken": "tok", "refreshToken": "rt", "expiresAt": 1},
            "account": {"uid": "u-1"}
        }))
        .expect("test credential parses");
        let path = dir.join("lobsterai-u-1.json");
        std::fs::write(&path, b"{}").expect("write placeholder credential");
        let credential = Credential::new(path, data, b"s");

        let prefix = b"event:error\ndata:x\n\n".to_vec();
        let rest: Vec<reqwest::Result<Bytes>> = vec![Ok(Bytes::from_static(b"data:{\"a\":1}\n\n"))];
        let started = StartedGeneration {
            model: "m".into(),
            credential,
            stream: Box::pin(futures_util::stream::iter(rest)),
            prefix: prefix.clone(),
            started_at: Instant::now(),
            failover_count: 0,
            refresh_retry: false,
        };
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let collected: Vec<u8> = runtime.block_on(async {
            let mut stream = started.into_byte_stream();
            let mut out = Vec::new();
            while let Some(chunk) = stream.next().await {
                out.extend_from_slice(&chunk.expect("chunk"));
            }
            out
        });
        let mut expected = prefix;
        expected.extend_from_slice(b"data:{\"a\":1}\n\n");
        assert_eq!(collected, expected, "prefix bytes are replayed verbatim");
    }
}
