//! HTTP server: routes, request orchestration, the streaming pump (watchdog +
//! pings), and non-stream aggregation. All retry invariants are enforced
//! before any byte reaches the client: once the stream is flowing there is
//! no replay, no failover, no regeneration.

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use secrecy::ExposeSecret;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::anthropic::request::convert_request;
use crate::anthropic::stream::StreamConverter;
use crate::anthropic::types as atypes;
use crate::error::ApiError;
use crate::lobsterai::pool::Pool;
use crate::lobsterai::upstream::{self, GenerateOutcome, StartedGeneration};
pub(crate) use crate::observability::InFlightGuard;
use crate::observability::{log_request_summary, Metrics, RequestSummary};
use crate::reasoning_shadow::ReasoningShadowStore;
use crate::session;
use crate::stream_watch::StreamTimeouts;

/// One SSE data line hard limit (guards against runaway upstream events).
pub(crate) const MAX_SSE_LINE_BYTES: usize = 8 * 1024 * 1024;
/// Downstream ping cadence (never counts as upstream semantic progress).
pub(crate) const PING_INTERVAL: Duration = Duration::from_secs(15);

pub struct AppState {
    pub config: crate::config::Config,
    pub models: crate::models::ModelRegistry,
    pub pool: Arc<Pool>,
    pub http: reqwest::Client,
    pub metrics: Arc<Metrics>,
    pub shadow: Arc<ReasoningShadowStore>,
    pub started_at: Instant,
    pub timeouts: StreamTimeouts,
    pub server_secret: Vec<u8>,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/models", get(models))
        .route("/metrics", get(metrics))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/admin/status", get(admin_status))
        .layer(axum::extract::DefaultBodyLimit::max(
            state.config.limits.max_body_bytes,
        ))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// auth
// ---------------------------------------------------------------------------

/// Local API key check (Authorization: Bearer or x-api-key).
fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(expected) = &state.config.server.api_key else {
        return Ok(());
    };
    let provided = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .or_else(|| {
            headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::trim)
                .filter(|v| !v.is_empty())
        });
    match provided {
        Some(key) if constant_time_eq(key.as_bytes(), expected.expose_secret().as_bytes()) => {
            Ok(())
        }
        _ => Err(ApiError::authentication("invalid or missing API key")),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

// ---------------------------------------------------------------------------
// routes
// ---------------------------------------------------------------------------

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(state): State<Arc<AppState>>) -> Response {
    if state.pool.is_empty() {
        (StatusCode::SERVICE_UNAVAILABLE, "no credentials loaded").into_response()
    } else {
        "ready".into_response()
    }
}

/// `GET /metrics` — Prometheus/OpenMetrics text exposition. Not behind
/// `check_auth`: the payload contains only aggregate counters/gauges.
async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    let (healthy, cooling) = crate::observability::pool_gauges(&state.pool);
    let gauges = crate::observability::ScrapeGauges {
        credentials_healthy: healthy,
        credentials_cooling: cooling,
        reasoning_shadow_entries: state
            .shadow
            .metrics
            .active_entries
            .load(std::sync::atomic::Ordering::Relaxed),
    };
    let body = crate::observability::render(
        &state.metrics,
        gauges,
        crate::VERSION,
        state.started_at.elapsed().as_secs(),
    );
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

/// `GET /v1/models` — Anthropic model discovery (Claude Code probes this
/// with the Anthropic SDK list shape).
async fn models(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(err) = check_auth(&state, &headers) {
        return err.into_response();
    }
    state.metrics.record_model_discovery();
    const CREATED_AT: &str = "2026-01-01T00:00:00Z";
    let data: Vec<Value> = state
        .models
        .definitions()
        .iter()
        .map(|definition| crate::models::model_list_entry(definition, CREATED_AT))
        .collect();
    let first_id = data
        .first()
        .and_then(|model| model["id"].as_str())
        .unwrap_or("")
        .to_owned();
    let last_id = data
        .last()
        .and_then(|model| model["id"].as_str())
        .unwrap_or("")
        .to_owned();
    Json(json!({
        "data": data,
        "first_id": first_id,
        "has_more": false,
        "last_id": last_id,
    }))
    .into_response()
}

async fn admin_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(err) = check_auth(&state, &headers) {
        return err.into_response();
    }
    let model = state.config.model.default.clone();
    let body = json!({
        "version": crate::VERSION,
        "uptime_secs": state.started_at.elapsed().as_secs(),
        "model": model,
        "metrics": state.metrics.snapshot(),
        "accounts": state.pool.snapshot(&state.config.model.default),
    });
    Json(body).into_response()
}

async fn count_tokens(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    if let Err(err) = check_auth(&state, &headers) {
        return err.into_response();
    }
    state.metrics.record_count_tokens();
    let (mut body, error) = read_json_body(request, state.config.limits.max_body_bytes).await;
    if let Some(err) = error {
        return err.into_response();
    }
    // The estimate sees exactly the normalizations the wire path applies.
    crate::deepseek::policy::strip_billing_in_anthropic_system(&mut body);
    crate::deepseek::policy::strip_anthropic_thinking(&mut body);
    let tokens = crate::deepseek::policy::estimate_anthropic_tokens(&body);
    (
        StatusCode::OK,
        [("x-lobsterai-proxy-token-count", "estimated-deepseek")],
        Json(json!({"input_tokens": tokens})),
    )
        .into_response()
}

async fn messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    if let Err(err) = check_auth(&state, &headers) {
        return err.into_response();
    }
    let (raw_body, error) = read_json_body(request, state.config.limits.max_body_bytes).await;
    if let Some(err) = error {
        state.metrics.record_error();
        return err.into_response();
    }
    state
        .metrics
        .record_bytes_in(serde_json::to_vec(&raw_body).map(|b| b.len()).unwrap_or(0) as u64);
    let started_at = Instant::now();
    let request_id = session::request_id();
    let session_fp = session::extract_raw_session(&raw_body)
        .map(|raw| session::fingerprint(&state.server_secret, raw));
    let client_stream = raw_body
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    state.metrics.record_request(client_stream);
    // RAII in-flight accounting: every return path decrements through `Drop`.
    let in_flight = InFlightGuard::new(state.metrics.clone(), client_stream);

    // --- convert Anthropic → OpenAI ---
    let mut anthropic_body = raw_body;
    let default_model = state.config.model.default.clone();
    let converted = match convert_request(&mut anthropic_body, &default_model) {
        Ok(converted) => converted,
        Err(err) => {
            state.metrics.record_error();
            let mut api_error = ApiError::invalid_request(err.message);
            api_error.request_id = Some(request_id);
            drop(in_flight);
            return api_error.into_response();
        }
    };

    // --- deepseek-v4.1-flash policy (lossless, prefix-stable) ---
    let mut chat_body = converted.chat_body;
    if let Some(fp) = &session_fp {
        // Reasoning shadow restore must happen before the historical strip.
        state
            .shadow
            .restore_into(chat_body.as_object_mut().unwrap(), fp);
    }
    let expose_thinking = converted.thinking_requested;
    let (historical_reasoning_removed, canonicalized_args) = {
        let object = chat_body.as_object_mut().expect("chat body is an object");
        let removed = crate::deepseek::policy::strip_historical_reasoning(object);
        let canonicalized = crate::deepseek::policy::canonicalize_tool_arguments(object);
        (removed, canonicalized)
    };
    let (prefix_hash, prefix_bytes) = {
        let object = chat_body.as_object().expect("chat body is an object");
        crate::deepseek::policy::stable_prefix_hash(object)
    };
    let chat_object = chat_body.as_object().expect("chat body is an object");
    let request_bytes = serde_json::to_string(chat_object)
        .map(|s| s.len())
        .unwrap_or(0);

    tracing::debug!(
        request_id = %request_id,
        session = session_fp.as_deref().unwrap_or("none"),
        prefix_hash = %prefix_hash,
        prefix_bytes,
        canonicalized_args,
        historical_reasoning_removed,
        "prefix telemetry"
    );

    // --- generation (retry invariants enforced inside) ---
    let outcome = upstream::generate(
        &state.pool,
        &state.http,
        &state.config,
        session_fp.as_deref(),
        &chat_body,
        &state.metrics,
    )
    .await;
    let started_generation = match outcome {
        GenerateOutcome::Started(started) => *started,
        GenerateOutcome::Failed(failure) => {
            state.metrics.record_error();
            state
                .metrics
                .request_duration_seconds
                .observe_ms(started_at.elapsed().as_millis());
            let mut api_error = ApiError::from(failure);
            api_error.request_id = Some(request_id.clone());
            log_summary_error(
                &state,
                &request_id,
                session_fp.as_deref(),
                &api_error,
                started_at,
                request_bytes,
            );
            drop(in_flight);
            return api_error.into_response();
        }
    };
    state
        .metrics
        .record_failover(started_generation.failover_count);

    let model = if started_generation.model.is_empty() {
        state.config.model.default.clone()
    } else {
        started_generation.model.clone()
    };
    let client = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_client_app)
        .map(str::to_owned);
    let mut summary = RequestSummary {
        request_id: request_id.clone(),
        session: session_fp.clone(),
        model,
        credential: Some(started_generation.credential.safe_name.clone()),
        stream: client_stream,
        http_status: 200,
        request_bytes,
        failovers: started_generation.failover_count,
        refresh_retry: started_generation.refresh_retry,
        client,
        ..RequestSummary::default()
    };

    if client_stream {
        // The pump's summary task owns `in_flight`: the request is only
        // complete once the stream has finished (or the client disconnected).
        return stream_response(
            &state,
            started_generation,
            expose_thinking,
            session_fp.clone(),
            started_at,
            &mut summary,
            in_flight,
        );
    }
    let result = aggregate_response(
        started_generation,
        expose_thinking,
        state.timeouts,
        state.shadow.clone(),
        session_fp.clone(),
        started_at,
        &mut summary,
        &state.metrics,
    )
    .await;
    summary.duration_ms = started_at.elapsed().as_millis();
    summary.http_status = if result.is_err() { 502 } else { 200 };
    state.metrics.record_tokens(
        summary.input_tokens,
        summary.output_tokens,
        summary.cached_tokens,
    );
    state.metrics.record_latencies(&summary);
    log_request_summary(&summary);
    drop(in_flight);
    match result {
        Ok(message) => {
            state.metrics.record_ok();
            Json(message).into_response()
        }
        Err(err) => {
            state.metrics.record_error();
            let mut api_error = err;
            api_error.request_id = Some(request_id);
            api_error.into_response()
        }
    }
}

/// Extract a low-cardinality client identifier from the User-Agent header
/// (`claude-cli/2.1.224 other/…` → `claude-cli/2.1.224`). Tracing only.
fn parse_client_app(user_agent: &str) -> Option<&str> {
    let app = user_agent.split_whitespace().next()?;
    (!app.is_empty()).then_some(app)
}

fn log_summary_error(
    state: &AppState,
    request_id: &str,
    session_fp: Option<&str>,
    error: &ApiError,
    started_at: Instant,
    request_bytes: usize,
) {
    let summary = RequestSummary {
        request_id: request_id.to_owned(),
        session: session_fp.map(ToOwned::to_owned),
        model: state.config.model.default.clone(),
        http_status: error.status,
        duration_ms: started_at.elapsed().as_millis(),
        request_bytes,
        error: Some(error.message.clone()),
        ..RequestSummary::default()
    };
    log_request_summary(&summary);
}

// ---------------------------------------------------------------------------
// body reading
// ---------------------------------------------------------------------------

async fn read_json_body(request: Request, limit: usize) -> (Value, Option<ApiError>) {
    let body = request.into_body();
    let bytes = match axum::body::to_bytes(body, limit).await {
        Ok(bytes) => bytes,
        Err(err) => {
            return (
                Value::Null,
                Some(ApiError::invalid_request(format!(
                    "failed to read request body: {err}"
                ))),
            )
        }
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => (value, None),
        Err(err) => (
            Value::Null,
            Some(ApiError::invalid_request(format!(
                "invalid JSON body: {err}"
            ))),
        ),
    }
}

// ---------------------------------------------------------------------------
// stream pump
// ---------------------------------------------------------------------------

pub(crate) enum PumpItem {
    Bytes(Vec<u8>),
    Finished,
}

#[derive(Debug, Default, Clone)]
pub struct StreamStats {
    pub ttft_ms: Option<u128>,
    pub first_reasoning_ms: Option<u128>,
    pub first_text_ms: Option<u128>,
    pub first_tool_ms: Option<u128>,
    pub reasoning_bytes: u64,
    pub text_bytes: u64,
    pub tool_bytes: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub finish_reason: Option<String>,
    pub duration_ms: u128,
}

/// Consumes the upstream SSE byte stream with watchdog + converter. Shared by
/// the streaming pump task and the non-stream aggregation loop.
struct StreamPump {
    converter: StreamConverter,
    watch: crate::stream_watch::StreamWatch,
    /// Request receipt time — all latency stats measure from here.
    request_started: Instant,
    buffer: Vec<u8>,
    stats: StreamStats,
}

impl StreamPump {
    fn new(expose_thinking: bool, timeouts: StreamTimeouts, request_started: Instant) -> Self {
        Self {
            converter: StreamConverter::new(expose_thinking),
            watch: crate::stream_watch::StreamWatch::new(timeouts, tokio::time::Instant::now()),
            request_started,
            buffer: Vec::with_capacity(8192),
            stats: StreamStats::default(),
        }
    }

    /// Feed upstream bytes; append emitted Anthropic SSE to `out`. Returns
    /// Some(()) when the logical stream completed ([DONE] seen).
    fn on_bytes(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> Option<()> {
        self.buffer.extend_from_slice(bytes);
        while let Some(newline) = self.buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=newline).collect();
            let line = trim_ascii(&line[..line.len() - 1]);
            if line.is_empty() {
                continue;
            }
            if line.len() > MAX_SSE_LINE_BYTES {
                // Oversized event: protocol failure; the drive loop will
                // finish with an error (finish_reason is still unset).
                continue;
            }
            self.watch.on_sse_event();
            let Some(payload) = line.strip_prefix(b"data:") else {
                continue;
            };
            let payload = trim_ascii(payload);
            if payload == b"[DONE]" {
                return Some(());
            }
            let Ok(chunk) = serde_json::from_slice::<Value>(payload) else {
                // Malformed event: tolerated, not semantic progress.
                continue;
            };
            let semantic = self.converter.feed_chunk(&chunk, out);
            if semantic {
                let elapsed = self.request_started.elapsed().as_millis();
                self.watch.on_semantic(tokio::time::Instant::now());
                if self.stats.ttft_ms.is_none() {
                    self.stats.ttft_ms = Some(elapsed);
                }
                if self.stats.first_reasoning_ms.is_none() && self.converter.has_reasoning() {
                    self.stats.first_reasoning_ms = Some(elapsed);
                }
                if self.stats.first_text_ms.is_none() && self.converter.has_text() {
                    self.stats.first_text_ms = Some(elapsed);
                }
                if self.stats.first_tool_ms.is_none() && self.converter.has_tools() {
                    self.stats.first_tool_ms = Some(elapsed);
                }
            }
        }
        None
    }

    /// Complete the logical stream: close blocks, emit trailing events,
    /// finalize stats, and apply shadow bookkeeping. `error` describes an
    /// abnormal termination (EOF without DONE, transport error, stall).
    fn finish(
        &mut self,
        error: Option<&str>,
        out: &mut Vec<u8>,
        shadow: Option<(&ReasoningShadowStore, &str)>,
    ) -> Value {
        self.stats.duration_ms = self.request_started.elapsed().as_millis();
        if self.converter.finish_reason().is_none() {
            // Abnormal termination: surface an Anthropic error event, then
            // close the stream so the client sees a well-formed end.
            let message = error.unwrap_or("upstream stream ended unexpectedly");
            out.extend_from_slice(atypes::sse_error(crate::error::API, message).as_bytes());
        }
        self.converter.finish(out);
        let accumulated = self.converter.accumulated();
        self.stats.finish_reason = accumulated.finish_reason.clone();
        if let Some(usage) = accumulated.usage {
            self.stats.input_tokens = Some(usage.0);
            self.stats.output_tokens = Some(usage.1);
            self.stats.cached_tokens = Some(usage.2);
        }
        self.stats.reasoning_bytes = accumulated.reasoning.len() as u64;
        self.stats.text_bytes = accumulated.text.len() as u64;
        self.stats.tool_bytes = accumulated
            .tool_calls
            .iter()
            .map(|tool_call| tool_call.arguments.len() as u64)
            .sum();
        // Shadow: keep this turn's reasoning while the tool loop continues;
        // clear the session when the turn ended with a final answer.
        if let Some((shadow, session_fp)) = shadow {
            let tool_ids: Vec<String> = accumulated
                .tool_calls
                .iter()
                .map(|tool_call: &crate::anthropic::stream::ToolCallAccum| tool_call.id.clone())
                .collect();
            if accumulated.finish_reason.as_deref() == Some("tool_calls") {
                shadow.store(session_fp, &tool_ids, &accumulated.reasoning);
            } else if accumulated.finish_reason.is_some() {
                shadow.clear_session(session_fp);
            }
        }
        self.converter.nonstream_response()
    }
}

pub(crate) fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && (bytes[start] == b' ' || bytes[start] == b'\t' || bytes[start] == b'\r') {
        start += 1;
    }
    while end > start
        && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t' || bytes[end - 1] == b'\r')
    {
        end -= 1;
    }
    &bytes[start..end]
}

/// Handle a stalled stream: emit the error event and finish.
pub(crate) fn stall_error(stall: crate::stream_watch::StallKind) -> String {
    format!("{}: {}", stall.as_str(), stall.message())
}

/// Streaming path: pump upstream SSE into downstream SSE bytes.
fn stream_response(
    state: &AppState,
    started: StartedGeneration,
    expose_thinking: bool,
    session_fp: Option<String>,
    started_at: Instant,
    summary: &mut RequestSummary,
    in_flight: InFlightGuard,
) -> Response {
    let timeouts = state.timeouts;
    let (tx, rx) = tokio::sync::mpsc::channel::<PumpItem>(64);
    let (stats_tx, stats_rx) = tokio::sync::oneshot::channel::<(StreamStats, Option<String>)>();
    let shadow = state.shadow.clone();
    let pump_metrics = state.metrics.clone();
    let response_stream = started.response.bytes_stream();
    // The pump task owns the in-flight guard: it terminates on every path —
    // normal completion, error, AND client disconnect.
    let (pump_done_tx, pump_done) = tokio::sync::oneshot::channel::<()>();
    let close_watch = tx.clone();

    // Detached task: completion observed via the pump_done channel.
    let _pump_task = tokio::spawn(async move {
        let metrics = pump_metrics;
        let _in_flight = in_flight;
        let mut pump = StreamPump::new(expose_thinking, timeouts, started_at);
        let mut byte_stream = response_stream;
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut out: Vec<u8> = Vec::with_capacity(4096);
        #[allow(unused_assignments)]
        let mut terminal: Option<(StreamStats, Option<String>)> = None;
        let shadow_ref = session_fp.clone().map(|fp| (shadow, fp));
        loop {
            tokio::select! {
                biased;
                _ = close_watch.closed() => {
                    // Client went away mid-stream: stop consuming upstream.
                    // The in-flight guard drops with this task.
                    let _ = pump_done_tx.send(());
                    return;
                }
                _ = ping.tick() => {
                    out.extend_from_slice(atypes::sse_ping().as_bytes());
                }
                _ = tokio::time::sleep_until(pump.watch.next_deadline()) => {
                    if let Some(stall) = pump.watch.check(tokio::time::Instant::now()) {
                        metrics.record_stream_stall(stall);
                        let message = stall_error(stall);
                        let _ = pump.finish(Some(&message), &mut out, shadow_ref.as_ref().map(|(s, fp)| (s.as_ref(), fp.as_str())));
                        let stats = pump.stats.clone();
                        terminal = Some((stats, Some(message)));
                        break;
                    }
                    // Spurious wake (no timer armed): keep waiting.
                }
                chunk = byte_stream.next() => {
                    match chunk {
                        None => {
                            let error = (pump.converter.finish_reason().is_none())
                                .then(|| "upstream stream ended unexpectedly".to_owned());
                            let _ = pump.finish(
                                error.as_deref(),
                                &mut out,
                                shadow_ref.as_ref().map(|(s, fp)| (s.as_ref(), fp.as_str())),
                            );
                            let stats = pump.stats.clone();
                            terminal = Some((stats, error));
                            break;
                        }
                        Some(Err(err)) => {
                            let message = format!("upstream stream error: {err}");
                            let _ = pump.finish(
                                Some(&message),
                                &mut out,
                                shadow_ref.as_ref().map(|(s, fp)| (s.as_ref(), fp.as_str())),
                            );
                            let stats = pump.stats.clone();
                            terminal = Some((stats, Some(message)));
                            break;
                        }
                        Some(Ok(bytes)) => {
                            pump.watch.on_upstream_bytes(tokio::time::Instant::now());
                            if pump.on_bytes(&bytes, &mut out).is_some() {
                                let _ = pump.finish(
                                    None,
                                    &mut out,
                                    shadow_ref.as_ref().map(|(s, fp)| (s.as_ref(), fp.as_str())),
                                );
                                let stats = pump.stats.clone();
                                terminal = Some((stats, None));
                                break;
                            }
                        }
                    }
                }
            }
            if !out.is_empty() {
                let payload = std::mem::take(&mut out);
                if tx.send(PumpItem::Bytes(payload)).await.is_err() {
                    // Client went away: stop consuming upstream.
                    return;
                }
            }
        }
        if !out.is_empty() {
            let _ = tx.send(PumpItem::Bytes(std::mem::take(&mut out))).await;
        }
        let _ = tx.send(PumpItem::Finished).await;
        if let Some((stats, error)) = terminal {
            let _ = stats_tx.send((stats, error));
        }
        let _ = pump_done_tx.send(());
    });

    // The pump task is the single summary logger for the streaming path.
    let mut summary = std::mem::take(summary);
    let metrics = state.metrics.clone();
    let _summary_task = tokio::spawn(async move {
        let (stats, error) = match pump_done.await {
            Ok(()) => match stats_rx.await {
                Ok(result) => result,
                Err(_) => return, // no terminal state was sent (disconnect)
            },
            Err(_) => return,
        };
        summary.ttft_ms = stats.ttft_ms;
        summary.first_reasoning_ms = stats.first_reasoning_ms;
        summary.first_text_ms = stats.first_text_ms;
        summary.first_tool_ms = stats.first_tool_ms;
        summary.reasoning_bytes = stats.reasoning_bytes;
        summary.text_bytes = stats.text_bytes;
        summary.tool_bytes = stats.tool_bytes;
        summary.input_tokens = stats.input_tokens;
        summary.output_tokens = stats.output_tokens;
        summary.cached_tokens = stats.cached_tokens;
        summary.finish_reason = stats.finish_reason;
        summary.duration_ms = stats.duration_ms;
        summary.error = error;
        summary.http_status = if summary.error.is_some() { 502 } else { 200 };
        if summary.error.is_some() {
            metrics.record_error();
        } else {
            metrics.record_ok();
        }
        metrics.record_bytes_out(summary.text_bytes + summary.reasoning_bytes + summary.tool_bytes);
        metrics.record_tokens(
            summary.input_tokens,
            summary.output_tokens,
            summary.cached_tokens,
        );
        metrics.record_latencies(&summary);
        log_request_summary(&summary);
    });

    let byte_stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Some(PumpItem::Bytes(bytes)) => {
                    return Some((Ok::<_, std::convert::Infallible>(Bytes::from(bytes)), rx));
                }
                Some(PumpItem::Finished) => continue,
                None => return None,
            }
        }
    });
    let mut response = axum::body::Body::from_stream(byte_stream).into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/event-stream"),
    );
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-cache"),
    );
    response.headers_mut().insert(
        axum::http::header::HeaderName::from_static("x-accel-buffering"),
        axum::http::HeaderValue::from_static("no"),
    );
    response
}

/// Non-stream path: ONE upstream stream, aggregated locally into a single
/// Anthropic Message JSON. Never re-generates.
#[allow(clippy::too_many_arguments)]
async fn aggregate_response(
    started: StartedGeneration,
    expose_thinking: bool,
    timeouts: StreamTimeouts,
    shadow: Arc<ReasoningShadowStore>,
    session_fp: Option<String>,
    started_at: Instant,
    summary: &mut RequestSummary,
    metrics: &Metrics,
) -> Result<Value, ApiError> {
    let mut pump = StreamPump::new(expose_thinking, timeouts, started_at);
    let mut byte_stream = started.response.bytes_stream();
    let mut out: Vec<u8> = Vec::with_capacity(4096);
    let shadow_pair = session_fp.clone().map(|fp| (shadow, fp));
    loop {
        let deadline = pump.watch.next_deadline();
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                if let Some(stall) = pump.watch.check(tokio::time::Instant::now()) {
                    metrics.record_stream_stall(stall);
                    let message = stall_error(stall);
                    let _ = pump.finish(Some(&message), &mut out, shadow_pair.as_ref().map(|(s, fp)| (s.as_ref(), fp.as_str())));
                    copy_stats(&pump.stats, summary);
                    return Err(ApiError::upstream(504, message));
                }
            }
            chunk = byte_stream.next() => {
                match chunk {
                    None => {
                        let error = (pump.converter.finish_reason().is_none())
                            .then(|| "upstream stream ended unexpectedly".to_owned());
                        let message = pump.finish(
                            error.as_deref(),
                            &mut out,
                            shadow_pair.as_ref().map(|(s, fp)| (s.as_ref(), fp.as_str())),
                        );
                        copy_stats(&pump.stats, summary);
                        if let Some(error) = error {
                            return Err(ApiError::upstream(502, error));
                        }
                        return Ok(message);
                    }
                    Some(Err(err)) => {
                        let message = format!("upstream stream error: {err}");
                        let _ = pump.finish(
                            Some(&message),
                            &mut out,
                            shadow_pair.as_ref().map(|(s, fp)| (s.as_ref(), fp.as_str())),
                        );
                        copy_stats(&pump.stats, summary);
                        return Err(ApiError::upstream(502, message));
                    }
                    Some(Ok(bytes)) => {
                        pump.watch.on_upstream_bytes(tokio::time::Instant::now());
                        if pump.on_bytes(&bytes, &mut out).is_some() {
                            let message = pump.finish(
                                None,
                                &mut out,
                                shadow_pair.as_ref().map(|(s, fp)| (s.as_ref(), fp.as_str())),
                            );
                            copy_stats(&pump.stats, summary);
                            return Ok(message);
                        }
                    }
                }
            }
        }
    }
}

fn copy_stats(stats: &StreamStats, summary: &mut RequestSummary) {
    summary.ttft_ms = stats.ttft_ms;
    summary.first_reasoning_ms = stats.first_reasoning_ms;
    summary.first_text_ms = stats.first_text_ms;
    summary.first_tool_ms = stats.first_tool_ms;
    summary.reasoning_bytes = stats.reasoning_bytes;
    summary.text_bytes = stats.text_bytes;
    summary.tool_bytes = stats.tool_bytes;
    summary.input_tokens = stats.input_tokens;
    summary.output_tokens = stats.output_tokens;
    summary.cached_tokens = stats.cached_tokens;
    summary.finish_reason = stats.finish_reason.clone();
    summary.duration_ms = stats.duration_ms;
}

// ---------------------------------------------------------------------------
// bootstrap (serve / status)
// ---------------------------------------------------------------------------

/// Common bootstrap: load accounts, build pool/client/shadow/state.
pub fn build_state(config: crate::config::Config) -> Result<AppState, anyhow::Error> {
    let server_secret = config.server_secret();
    let pool = Arc::new(Pool::new(server_secret.clone(), config.limits));
    pool.load_dir(&config.auth.dir);

    let shadow = Arc::new(ReasoningShadowStore::new(
        crate::reasoning_shadow::ShadowLimits {
            max_sessions: config.limits.shadow_max_sessions,
            ttl: Duration::from_secs(config.limits.shadow_ttl_secs),
            ..crate::reasoning_shadow::ShadowLimits::default()
        },
    ));
    let timeouts = config.timeouts();
    let http = crate::lobsterai::upstream::build_client(&config.upstream);

    Ok(AppState {
        models: crate::models::ModelRegistry::new(),
        pool,
        http,
        metrics: Arc::new(Metrics::default()),
        shadow,
        started_at: Instant::now(),
        timeouts,
        server_secret,
        config,
    })
}

/// Human-readable status for the `status` subcommand (no secrets).
pub fn print_status(state: &AppState) {
    let accounts = state.pool.snapshot(&state.config.model.default);
    println!("model      : {}", state.config.model.default);
    println!("auth dir   : {}", state.config.auth.dir.display());
    println!("accounts   : {}", accounts.len());
    for account in accounts {
        println!(
            "  - {} healthy={} credits={} token_expires_at_secs={}",
            account.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
            account
                .get("healthy")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            account
                .get("credits")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1),
            account
                .get("token_expires_at_secs")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
        );
    }
}

/// Start the proxy server with graceful shutdown and background housekeeping.
pub async fn serve(config: crate::config::Config) -> Result<(), anyhow::Error> {
    use anyhow::Context as _;

    let state = Arc::new(build_state(config)?);
    if state.pool.is_empty() {
        tracing::warn!(
            "no accounts loaded; run `lobsterai-proxy login` or place lobsterai-*.json files in {}",
            state.config.auth.dir.display()
        );
    } else {
        tracing::info!(accounts = state.pool.len(), "account pool ready");
    }

    // One shutdown signal fans out to every background task and to the drain.
    let (shutdown_tx, _shutdown_rx) = crate::shutdown::signal_channel();
    let housekeep_shutdown = shutdown_tx.subscribe();
    let serve_shutdown = shutdown_tx.subscribe();
    std::mem::drop(_shutdown_rx);

    // Background housekeeping (refresh-due + daily check-in/credits) —
    // wired in the check-in PR; the pool refreshes lazily on demand until then.
    let housekeep_pool = state.pool.clone();
    let housekeep_http = state.http.clone();
    let base_url = state.config.upstream.base_url.clone();
    let margin = state.config.limits.refresh_margin_secs as i64;
    let keepalive = state.config.limits.keepalive_secs;
    let _housekeep_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            housekeep_pool.prune();
            let files = crate::lobsterai::pool::list_credential_files(&housekeep_pool_auth_dir());
            housekeep_pool.load_files(&files);
            housekeep_pool
                .refresh_due(&housekeep_http, &base_url, margin, keepalive, None)
                .await;
        }
    });
    let _ = &housekeep_shutdown;

    let addr = format!("{}:{}", state.config.server.host, state.config.server.port);
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    let drain_timeout = state.config.shutdown_timeout();
    tracing::info!(
        version = crate::VERSION,
        %addr,
        model = %state.config.model.default,
        shutdown_timeout_secs = drain_timeout.as_secs(),
        "lobsterai-proxy listening"
    );

    let serve = std::future::IntoFuture::into_future(
        axum::serve(listener, app)
            .with_graceful_shutdown(crate::shutdown::shutdown_requested(serve_shutdown)),
    );
    let serve = async {
        let result = serve.await;
        result
    };
    let outcome = crate::shutdown::drain_until_idle(
        state.metrics.clone(),
        serve,
        drain_timeout,
        shutdown_tx.subscribe(),
    )
    .await;
    tracing::info!(?outcome, "server stopped");
    Ok(())
}

/// Auth dir for the refresh loop: read from the config captured by serve.
/// (The loop task captures only the pool; the auth dir lives on the config.)
fn housekeep_pool_auth_dir() -> std::path::PathBuf {
    std::env::var("LOBSTERAI_AUTH_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("auth"))
}
