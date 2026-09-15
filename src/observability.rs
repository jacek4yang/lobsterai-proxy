//! Observability: global counters, a Prometheus/OpenMetrics text renderer for
//! `GET /metrics`, and one compact, redacted summary line per request. Never
//! logs prompt content, tool output, reasoning text, tokens, or raw
//! session/credential identifiers.
//!
//! Metric hygiene rules (enforced by review, not by the type system):
//! - No high-cardinality labels: no request ids, session fingerprints,
//!   credential names, UIDs, tool names, IPs, models, or error strings ever
//!   appear as label values. The only labels are small fixed enums
//!   (`result`, `kind`).
//! - No secrets in metric names, labels, or `# HELP` text.
//! - Counters are monotonic `AtomicU64`s; gauges are either atomics updated on
//!   the event (in-flight) or computed at scrape time from a pool snapshot
//!   (credentials_healthy / credentials_cooling / reasoning_shadow_entries).

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use crate::lobsterai::pool::Pool;

/// Latency histogram bucket upper bounds in seconds (Prometheus defaults).
pub const HISTOGRAM_BUCKETS_SECONDS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Minimal fixed-bucket histogram: one counter per bucket (cumulative `_bucket`
/// lines are derived at render time) plus `_sum`/`_count`.
///
/// Hand-rolled on purpose: a scrape-time renderer needs no background registry,
/// no global state, and no new dependency.
pub struct Histogram {
    buckets: [AtomicU64; HISTOGRAM_BUCKETS_SECONDS.len()],
    /// Sum of observations in microseconds (integer-only; rendered as seconds).
    sum_micros: AtomicU64,
    count: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: Default::default(),
            sum_micros: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }
}

impl Histogram {
    /// Record a latency in milliseconds. Zero/`None` callers must skip: an
    /// unobserved latency is not a zero-latency observation.
    pub fn observe_ms(&self, millis: u128) {
        let seconds = millis as f64 / 1000.0;
        let mut index = HISTOGRAM_BUCKETS_SECONDS.len();
        for (position, bound) in HISTOGRAM_BUCKETS_SECONDS.iter().enumerate() {
            if seconds <= *bound {
                index = position;
                break;
            }
        }
        if index < self.buckets.len() {
            self.buckets[index].fetch_add(1, Ordering::Relaxed);
        }
        self.sum_micros.fetch_add(
            millis.saturating_mul(1000).min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    fn sum_seconds(&self) -> f64 {
        self.sum_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0
    }

    fn bucket_counts(&self) -> [u64; HISTOGRAM_BUCKETS_SECONDS.len()] {
        let mut cumulative = [0u64; HISTOGRAM_BUCKETS_SECONDS.len()];
        let mut running = 0u64;
        for (index, bucket) in self.buckets.iter().enumerate() {
            running += bucket.load(Ordering::Relaxed);
            cumulative[index] = running;
        }
        cumulative
    }
}

/// Global counters; cheap to clone for background tasks.
#[derive(Default)]
pub struct Metrics {
    pub requests_total: AtomicU64,
    pub requests_stream: AtomicU64,
    pub requests_ok: AtomicU64,
    pub requests_error: AtomicU64,
    pub upstream_401: AtomicU64,
    pub upstream_403: AtomicU64,
    pub upstream_429: AtomicU64,
    pub upstream_5xx: AtomicU64,
    pub web_search_tool_calls: AtomicU64,
    pub web_search_errors: AtomicU64,
    pub server_tool_rounds: AtomicU64,
    pub mixed_tool_rounds: AtomicU64,
    pub pause_turn_events: AtomicU64,
    pub count_tokens_requests: AtomicU64,
    pub model_discovery_requests: AtomicU64,
    pub transport_errors: AtomicU64,
    pub refresh_success: AtomicU64,
    pub refresh_failure: AtomicU64,
    pub failovers: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    /// Requests accepted but not yet answered (signed: an operator bug or a
    /// panic path may overshoot; the gauge must not wrap).
    pub requests_in_flight: AtomicI64,
    pub streams_in_flight: AtomicI64,
    pub input_tokens_total: AtomicU64,
    pub output_tokens_total: AtomicU64,
    pub cached_tokens_total: AtomicU64,
    /// Stall counters, one slot per [`crate::stream_watch::StallKind`]:
    /// 0 first_event, 1 first_semantic, 2 stream_idle, 3 semantic_idle.
    pub stream_stalls: [AtomicU64; 4],
    pub request_duration_seconds: Histogram,
    pub ttft_seconds: Histogram,
    pub first_reasoning_seconds: Histogram,
    pub first_text_seconds: Histogram,
    pub first_tool_seconds: Histogram,
}

impl Metrics {
    pub fn record_request(&self, stream: bool) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        if stream {
            self.requests_stream.fetch_add(1, Ordering::Relaxed);
        }
    }
    /// Request-level success: exactly one call per completed request, paired
    /// with [`Self::record_request`].
    pub fn record_ok(&self) {
        self.requests_ok.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_error(&self) {
        self.requests_error.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_upstream_401(&self) {
        self.upstream_401.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_upstream_403(&self) {
        self.upstream_403.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_upstream_429(&self) {
        self.upstream_429.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_upstream_5xx(&self) {
        self.upstream_5xx.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_transport_error(&self) {
        self.transport_errors.fetch_add(1, Ordering::Relaxed);
    }
    /// A completed token refresh attempt; `success` drives the `result` label.
    pub fn record_refresh(&self, success: bool) {
        if success {
            self.refresh_success.fetch_add(1, Ordering::Relaxed);
        } else {
            self.refresh_failure.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn record_web_search(&self) {
        self.web_search_tool_calls.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_web_search_error(&self) {
        self.web_search_errors.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_server_tool_round(&self) {
        self.server_tool_rounds.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_mixed_tool_round(&self) {
        self.mixed_tool_rounds.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_pause_turn(&self) {
        self.pause_turn_events.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_count_tokens(&self) {
        self.count_tokens_requests.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_model_discovery(&self) {
        self.model_discovery_requests
            .fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_failover(&self, count: u32) {
        self.failovers.fetch_add(count as u64, Ordering::Relaxed);
    }
    pub fn record_bytes_in(&self, n: u64) {
        self.bytes_in.fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_bytes_out(&self, n: u64) {
        self.bytes_out.fetch_add(n, Ordering::Relaxed);
    }
    /// A stalled stream, classified by [`crate::stream_watch::StallKind`].
    pub fn record_stream_stall(&self, kind: crate::stream_watch::StallKind) {
        let index = match kind {
            crate::stream_watch::StallKind::FirstEvent => 0,
            crate::stream_watch::StallKind::FirstSemantic => 1,
            crate::stream_watch::StallKind::StreamIdle => 2,
            crate::stream_watch::StallKind::SemanticIdle => 3,
        };
        self.stream_stalls[index].fetch_add(1, Ordering::Relaxed);
    }
    /// Token accounting from a finalized [`StreamStats`]; `None` fields were
    /// not reported by upstream and must not be counted as zero.
    pub fn record_tokens(&self, input: Option<u64>, output: Option<u64>, cached: Option<u64>) {
        if let Some(input) = input {
            self.input_tokens_total.fetch_add(input, Ordering::Relaxed);
        }
        if let Some(output) = output {
            self.output_tokens_total
                .fetch_add(output, Ordering::Relaxed);
        }
        if let Some(cached) = cached {
            self.cached_tokens_total
                .fetch_add(cached, Ordering::Relaxed);
        }
    }

    /// Latency histograms from a finalized request summary. Unobserved
    /// latencies (`None`) are skipped �?an absent measurement is not a zero.
    pub fn record_latencies(&self, summary: &RequestSummary) {
        self.request_duration_seconds
            .observe_ms(summary.duration_ms);
        if let Some(ttft) = summary.ttft_ms {
            self.ttft_seconds.observe_ms(ttft);
        }
        if let Some(first) = summary.first_reasoning_ms {
            self.first_reasoning_seconds.observe_ms(first);
        }
        if let Some(first) = summary.first_text_ms {
            self.first_text_seconds.observe_ms(first);
        }
        if let Some(first) = summary.first_tool_ms {
            self.first_tool_seconds.observe_ms(first);
        }
    }

    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "requests_total": self.requests_total.load(Ordering::Relaxed),
            "requests_stream": self.requests_stream.load(Ordering::Relaxed),
            "requests_ok": self.requests_ok.load(Ordering::Relaxed),
            "requests_error": self.requests_error.load(Ordering::Relaxed),
            "requests_in_flight": self.requests_in_flight.load(Ordering::Relaxed),
            "streams_in_flight": self.streams_in_flight.load(Ordering::Relaxed),
            "upstream_401": self.upstream_401.load(Ordering::Relaxed),
            "upstream_403": self.upstream_403.load(Ordering::Relaxed),
            "upstream_429": self.upstream_429.load(Ordering::Relaxed),
            "upstream_5xx": self.upstream_5xx.load(Ordering::Relaxed),
            "transport_errors": self.transport_errors.load(Ordering::Relaxed),
            "token_refreshes": self.refresh_success.load(Ordering::Relaxed),
            "refresh_failures": self.refresh_failure.load(Ordering::Relaxed),
            "credential_failovers": self.failovers.load(Ordering::Relaxed),
            "input_tokens_total": self.input_tokens_total.load(Ordering::Relaxed),
            "output_tokens_total": self.output_tokens_total.load(Ordering::Relaxed),
            "cached_tokens_total": self.cached_tokens_total.load(Ordering::Relaxed),
            "bytes_in": self.bytes_in.load(Ordering::Relaxed),
            "bytes_out": self.bytes_out.load(Ordering::Relaxed),
        })
    }
}

/// RAII in-flight guard: exactly one decrement per accept, on every exit path
/// (normal return, error return, streaming completion in a background task,
/// client disconnect, cancellation, panic). Leaking the gauge is the failure
/// mode this type exists to make impossible.
pub struct InFlightGuard {
    metrics: std::sync::Arc<Metrics>,
    stream: bool,
}

impl InFlightGuard {
    pub fn new(metrics: std::sync::Arc<Metrics>, stream: bool) -> Self {
        metrics.requests_in_flight.fetch_add(1, Ordering::Relaxed);
        if stream {
            metrics.streams_in_flight.fetch_add(1, Ordering::Relaxed);
        }
        Self { metrics, stream }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.metrics
            .requests_in_flight
            .fetch_sub(1, Ordering::Relaxed);
        if self.stream {
            self.metrics
                .streams_in_flight
                .fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Scrape-time gauge values that need live state (not atomics we maintain).
#[derive(Debug, Default, Clone, Copy)]
pub struct ScrapeGauges {
    pub credentials_healthy: u64,
    pub credentials_cooling: u64,
    pub reasoning_shadow_entries: u64,
}

/// Renders the whole `/metrics` document. Kept free of secrets and
/// high-cardinality labels by construction: every literal here is a static
/// name/label, and the only dynamic values are numbers.
pub fn render(metrics: &Metrics, gauges: ScrapeGauges, version: &str, uptime_secs: u64) -> String {
    let mut out = String::with_capacity(4096);

    // --- process info -----------------------------------------------------
    help_type(
        &mut out,
        "lobsterai_proxy_info",
        "Static build information (always 1).",
        "gauge",
    );
    out.push_str(&format!(
        "lobsterai_proxy_info{{version=\"{}\"}} 1\n",
        escape_label(version)
    ));
    help_type(
        &mut out,
        "lobsterai_proxy_uptime_seconds",
        "Seconds since this process started.",
        "gauge",
    );
    out.push_str(&format!("lobsterai_proxy_uptime_seconds {uptime_secs}\n"));

    // --- request counters -------------------------------------------------
    counter(
        &mut out,
        "lobsterai_proxy_requests_total",
        "Accepted POST /v1/messages requests.",
        metrics.requests_total.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "lobsterai_proxy_requests_stream_total",
        "Accepted requests that requested client-side streaming.",
        metrics.requests_stream.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "lobsterai_proxy_requests_ok_total",
        "Requests answered successfully (upstream HTTP 200 accepted).",
        metrics.requests_ok.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "lobsterai_proxy_requests_error_total",
        "Requests that failed locally or upstream.",
        metrics.requests_error.load(Ordering::Relaxed),
    );

    // --- in-flight gauges -------------------------------------------------
    gauge(
        &mut out,
        "lobsterai_proxy_requests_in_flight",
        "Requests currently being served (accepted, response not yet complete).",
        metrics.requests_in_flight.load(Ordering::Relaxed),
    );
    gauge(
        &mut out,
        "lobsterai_proxy_streams_in_flight",
        "Requests currently being served on the streaming path.",
        metrics.streams_in_flight.load(Ordering::Relaxed),
    );

    // --- upstream error counters -----------------------------------------
    counter(
        &mut out,
        "lobsterai_proxy_upstream_401_total",
        "Upstream HTTP 401 responses observed.",
        metrics.upstream_401.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "lobsterai_proxy_upstream_403_total",
        "Upstream HTTP 403 responses observed.",
        metrics.upstream_403.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "lobsterai_proxy_upstream_429_total",
        "Upstream HTTP 429 responses observed.",
        metrics.upstream_429.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "lobsterai_proxy_upstream_5xx_total",
        "Upstream HTTP 5xx responses observed.",
        metrics.upstream_5xx.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "lobsterai_proxy_transport_errors_total",
        "Upstream connection/transport failures before any response.",
        metrics.transport_errors.load(Ordering::Relaxed),
    );

    // --- server-side web search ------------------------------------------
    counter(
        &mut out,
        "lobsterai_proxy_web_search_tool_calls_total",
        "Server-side web searches executed for web_search server-tool rounds.",
        metrics.web_search_tool_calls.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "lobsterai_proxy_web_search_errors_total",
        "Server-side web searches that failed (reported to the model as errors).",
        metrics.web_search_errors.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "lobsterai_proxy_server_tool_rounds_total",
        "Upstream rounds fully executed server-side (continued upstream).",
        metrics.server_tool_rounds.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "lobsterai_proxy_mixed_tool_rounds_total",
        "Upstream rounds mixing server and client tools (server part executed).",
        metrics.mixed_tool_rounds.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "lobsterai_proxy_pause_turn_events_total",
        "Responses ended with stop_reason pause_turn for client resumption.",
        metrics.pause_turn_events.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "lobsterai_proxy_count_tokens_requests_total",
        "Token-count requests served.",
        metrics.count_tokens_requests.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "lobsterai_proxy_model_discovery_requests_total",
        "Model discovery (/v1/models) requests served.",
        metrics.model_discovery_requests.load(Ordering::Relaxed),
    );

    // --- credential lifecycle --------------------------------------------
    let refreshes = [
        ("success", metrics.refresh_success.load(Ordering::Relaxed)),
        ("failure", metrics.refresh_failure.load(Ordering::Relaxed)),
    ];
    help_type(
        &mut out,
        "lobsterai_proxy_refresh_total",
        "Access-token refresh attempts by result.",
        "counter",
    );
    for (result, value) in refreshes {
        out.push_str(&format!(
            "lobsterai_proxy_refresh_total{{result=\"{result}\"}} {value}\n"
        ));
    }
    counter(
        &mut out,
        "lobsterai_proxy_failover_total",
        "Credential failovers performed after a confirmed upstream 429.",
        metrics.failovers.load(Ordering::Relaxed),
    );
    gauge(
        &mut out,
        "lobsterai_proxy_credentials_healthy",
        "Credentials currently usable (not cooling, file present).",
        gauges.credentials_healthy as i64,
    );
    gauge(
        &mut out,
        "lobsterai_proxy_credentials_cooling",
        "Credentials currently in cooldown.",
        gauges.credentials_cooling as i64,
    );

    // --- stream stalls ----------------------------------------------------
    help_type(
        &mut out,
        "lobsterai_proxy_stream_stall_total",
        "Upstream streams abandoned by the stall watchdog, by kind.",
        "counter",
    );
    for (kind, slot) in STALL_KINDS.iter().zip(metrics.stream_stalls.iter()) {
        out.push_str(&format!(
            "lobsterai_proxy_stream_stall_total{{kind=\"{kind}\"}} {}\n",
            slot.load(Ordering::Relaxed)
        ));
    }

    // --- token counters ---------------------------------------------------
    counter(
        &mut out,
        "lobsterai_proxy_input_tokens_total",
        "Input tokens reported by upstream usage frames.",
        metrics.input_tokens_total.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "lobsterai_proxy_output_tokens_total",
        "Output tokens reported by upstream usage frames.",
        metrics.output_tokens_total.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "lobsterai_proxy_cached_tokens_total",
        "Prompt-cache read tokens reported by upstream usage frames.",
        metrics.cached_tokens_total.load(Ordering::Relaxed),
    );

    // --- bytes ------------------------------------------------------------
    counter(
        &mut out,
        "lobsterai_proxy_request_bytes_total",
        "Request body bytes accepted.",
        metrics.bytes_in.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "lobsterai_proxy_response_bytes_total",
        "Generated reasoning/text/tool bytes sent downstream.",
        metrics.bytes_out.load(Ordering::Relaxed),
    );

    // --- shadow store -----------------------------------------------------
    gauge(
        &mut out,
        "lobsterai_proxy_reasoning_shadow_entries",
        "Shadowed reasoning entries currently held in memory.",
        gauges.reasoning_shadow_entries as i64,
    );

    // --- latency histograms ----------------------------------------------
    histogram(
        &mut out,
        "lobsterai_proxy_request_duration_seconds",
        "End-to-end request duration in seconds.",
        &metrics.request_duration_seconds,
    );
    histogram(
        &mut out,
        "lobsterai_proxy_ttft_seconds",
        "Time to first semantic output (reasoning/text/tool) in seconds.",
        &metrics.ttft_seconds,
    );
    histogram(
        &mut out,
        "lobsterai_proxy_first_reasoning_seconds",
        "Time to first reasoning delta in seconds (observed streams only).",
        &metrics.first_reasoning_seconds,
    );
    histogram(
        &mut out,
        "lobsterai_proxy_first_text_seconds",
        "Time to first visible text delta in seconds (observed streams only).",
        &metrics.first_text_seconds,
    );
    histogram(
        &mut out,
        "lobsterai_proxy_first_tool_seconds",
        "Time to first tool-call delta in seconds (observed streams only).",
        &metrics.first_tool_seconds,
    );

    out
}

const STALL_KINDS: [&str; 4] = [
    "first_event",
    "first_semantic",
    "stream_idle",
    "semantic_idle",
];

fn help_type(out: &mut String, name: &str, help: &str, kind: &str) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
}

fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    help_type(out, name, help, "counter");
    out.push_str(&format!("{name} {value}\n"));
}

/// Gauges may legitimately go negative if an operator invariant is violated;
/// render the signed value rather than wrapping it.
fn gauge(out: &mut String, name: &str, help: &str, value: i64) {
    help_type(out, name, help, "gauge");
    out.push_str(&format!("{name} {value}\n"));
}

fn histogram(out: &mut String, name: &str, help: &str, histogram: &Histogram) {
    help_type(out, name, help, "histogram");
    let cumulative = histogram.bucket_counts();
    for (index, bound) in HISTOGRAM_BUCKETS_SECONDS.iter().enumerate() {
        out.push_str(&format!(
            "{name}_bucket{{le=\"{}\"}} {}\n",
            format_bound(*bound),
            cumulative[index]
        ));
    }
    out.push_str(&format!(
        "{name}_bucket{{le=\"+Inf\"}} {}\n",
        histogram.count()
    ));
    out.push_str(&format!(
        "{name}_sum {}\n{name}_count {}\n",
        format_sum(histogram.sum_seconds()),
        histogram.count()
    ));
}

/// Bucket bounds must render as the exact literals Prometheus expects.
fn format_bound(bound: f64) -> String {
    if bound.fract() == 0.0 {
        format!("{bound:.0}")
    } else {
        format!("{bound}")
    }
}

fn format_sum(seconds: f64) -> String {
    format!("{seconds:.6}")
}

/// Escape a label value per the Prometheus text format. Only used for the
/// build version, which is a crate constant �?never for user data.
fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Scrape-time account gauges: healthy vs cooling, from the pool's safe
/// snapshot (`/admin/status` shape) so no account identifier is exposed.
pub fn pool_gauges(pool: &Pool) -> (u64, u64) {
    let snapshot = pool.snapshot("");
    let total = snapshot.len() as u64;
    let healthy = snapshot
        .iter()
        .filter(|entry| {
            entry
                .get("healthy")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .count() as u64;
    (healthy, total.saturating_sub(healthy))
}

/// Per-request summary (sizes, counts, latencies only).
#[derive(Debug, Default, Clone)]
pub struct RequestSummary {
    pub request_id: String,
    pub session: Option<String>,
    pub model: String,
    pub credential: Option<String>,
    pub stream: bool,
    pub http_status: u16,
    pub duration_ms: u128,
    pub ttft_ms: Option<u128>,
    pub first_reasoning_ms: Option<u128>,
    pub first_text_ms: Option<u128>,
    pub first_tool_ms: Option<u128>,
    pub request_bytes: usize,
    pub system_bytes: usize,
    pub messages_bytes: usize,
    pub tools_bytes: usize,
    pub reasoning_bytes: u64,
    pub text_bytes: u64,
    pub tool_bytes: u64,
    pub cached_tokens: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub failovers: u32,
    pub refresh_retry: bool,
    pub finish_reason: Option<String>,
    pub web_searches: u64,
    /// Client application/version from the User-Agent (low cardinality,
    /// e.g. `claude-cli/2.1.224`). Tracing only �?never an auth input.
    pub client: Option<String>,
    pub error: Option<String>,
}

pub fn log_request_summary(summary: &RequestSummary) {
    tracing::info!(
        request_id = %summary.request_id,
        session = summary.session.as_deref().unwrap_or("none"),
        model = %summary.model,
        credential = summary.credential.as_deref().unwrap_or("none"),
        stream = summary.stream,
        status = summary.http_status,
        duration_ms = summary.duration_ms as u64,
        ttft_ms = summary.ttft_ms.map(|v| v as u64),
        first_reasoning_ms = summary.first_reasoning_ms.map(|v| v as u64),
        first_text_ms = summary.first_text_ms.map(|v| v as u64),
        first_tool_ms = summary.first_tool_ms.map(|v| v as u64),
        request_bytes = summary.request_bytes,
        system_bytes = summary.system_bytes,
        messages_bytes = summary.messages_bytes,
        tools_bytes = summary.tools_bytes,
        reasoning_bytes = summary.reasoning_bytes,
        text_bytes = summary.text_bytes,
        tool_bytes = summary.tool_bytes,
        cached_tokens = summary.cached_tokens,
        input_tokens = summary.input_tokens,
        output_tokens = summary.output_tokens,
        failovers = summary.failovers,
        refresh_retry = summary.refresh_retry,
        web_searches = summary.web_searches,
        client = summary.client.as_deref().unwrap_or("unknown"),
        finish = summary.finish_reason.as_deref().unwrap_or("none"),
        error = summary.error.as_deref().unwrap_or(""),
        "request summary"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_metrics() -> Metrics {
        let metrics = Metrics::default();
        metrics.record_request(true);
        metrics.record_request(false);
        metrics.record_ok();
        metrics.record_error();
        metrics.record_upstream_401();
        metrics.record_upstream_403();
        metrics.record_upstream_429();
        metrics.record_upstream_5xx();
        metrics.record_transport_error();
        metrics.record_refresh(true);
        metrics.record_refresh(false);
        metrics.record_failover(2);
        metrics.record_stream_stall(crate::stream_watch::StallKind::FirstEvent);
        metrics.record_stream_stall(crate::stream_watch::StallKind::SemanticIdle);
        metrics.record_tokens(Some(100), Some(10), Some(40));
        metrics.record_latencies(&RequestSummary {
            duration_ms: 1234,
            ttft_ms: Some(250),
            first_reasoning_ms: Some(250),
            first_text_ms: Some(300),
            first_tool_ms: Some(900),
            ..RequestSummary::default()
        });
        metrics
    }

    /// Every sample line must be either a bare `name value` or a single
    /// low-cardinality `name{label="value"} value` �?nothing else. Hand-rolled
    /// (no `regex` dependency) but equivalent to
    /// `^[a-z_]+(\{[a-z_]+="[a-z0-9_]+"\})? -?[0-9]+(\.[0-9]+)?$`.
    fn assert_valid_sample_lines(body: &str) {
        for line in body.lines() {
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let (series, value) = line
                .rsplit_once(' ')
                .unwrap_or_else(|| panic!("sample line has no value: {line}"));
            assert!(is_number(value), "sample value is not numeric: {line}");
            let (name, labels) = match series.split_once('{') {
                None => (series, None),
                Some((name, rest)) => {
                    let labels = rest
                        .strip_suffix('}')
                        .unwrap_or_else(|| panic!("unterminated label set: {line}"));
                    assert!(
                        !labels.contains('{') && !labels.contains('}'),
                        "nested braces in label set: {line}"
                    );
                    (name, Some(labels))
                }
            };
            assert!(
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                    && !name.as_bytes()[0].is_ascii_digit(),
                "metric name is not [a-z_][a-z0-9_]*: {line}"
            );
            if let Some(labels) = labels {
                for pair in labels.split(',') {
                    let (key, raw) = pair
                        .split_once("=\"")
                        .unwrap_or_else(|| panic!("malformed label pair: {line}"));
                    let raw = raw
                        .strip_suffix('"')
                        .unwrap_or_else(|| panic!("unterminated label value: {line}"));
                    // High-cardinality values (ids, names, paths, UIDs) fail
                    // this: label values are restricted to lowercase enums.
                    assert!(
                        !key.is_empty() && key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                        "label key is not [a-z_]+: {line}"
                    );
                    // High-cardinality values (ids, names, paths, UIDs) fail
                    // this: label values are restricted to lowercase enums.
                    // Two documented non-enum exceptions, both compile-time
                    // constants rather than user data: the build `version`
                    // and numeric histogram `le` bounds.
                    let numeric = !raw.is_empty()
                        && raw
                            .bytes()
                            .all(|b| b.is_ascii_digit() || b == b'.' || b == b'-' || b == b'+');
                    let allowed = if key == "version" {
                        numeric
                    } else if key == "le" {
                        // Histogram bounds: a number, or the literal `+Inf`.
                        numeric || raw == "+Inf"
                    } else {
                        !raw.is_empty()
                            && raw
                                .bytes()
                                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                    };
                    assert!(
                        allowed,
                        "label value is not [a-z0-9_]+ (high cardinality?): {line}"
                    );
                }
            }
        }
    }

    fn is_number(value: &str) -> bool {
        let digits = value.strip_prefix('-').unwrap_or(value);
        match digits.split_once('.') {
            None => !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()),
            Some((whole, fraction)) => {
                !whole.is_empty()
                    && whole.bytes().all(|b| b.is_ascii_digit())
                    && !fraction.is_empty()
                    && fraction.bytes().all(|b| b.is_ascii_digit())
            }
        }
    }

    #[test]
    fn render_is_openmetrics_shaped() {
        let metrics = fresh_metrics();
        let body = render(
            &metrics,
            ScrapeGauges {
                credentials_healthy: 3,
                credentials_cooling: 1,
                reasoning_shadow_entries: 7,
            },
            crate::VERSION,
            42,
        );
        assert_valid_sample_lines(&body);

        // Counters and gauges are present with the documented names.
        for expected in [
            "lobsterai_proxy_requests_total 2",
            "lobsterai_proxy_requests_stream_total 1",
            "lobsterai_proxy_requests_ok_total 1",
            "lobsterai_proxy_requests_error_total 1",
            "lobsterai_proxy_upstream_401_total 1",
            "lobsterai_proxy_upstream_403_total 1",
            "lobsterai_proxy_upstream_429_total 1",
            "lobsterai_proxy_upstream_5xx_total 1",
            "lobsterai_proxy_transport_errors_total 1",
            "lobsterai_proxy_refresh_total{result=\"success\"} 1",
            "lobsterai_proxy_refresh_total{result=\"failure\"} 1",
            "lobsterai_proxy_failover_total 2",
            "lobsterai_proxy_stream_stall_total{kind=\"first_event\"} 1",
            "lobsterai_proxy_stream_stall_total{kind=\"semantic_idle\"} 1",
            "lobsterai_proxy_input_tokens_total 100",
            "lobsterai_proxy_output_tokens_total 10",
            "lobsterai_proxy_cached_tokens_total 40",
            "lobsterai_proxy_credentials_healthy 3",
            "lobsterai_proxy_credentials_cooling 1",
            "lobsterai_proxy_reasoning_shadow_entries 7",
            "lobsterai_proxy_uptime_seconds 42",
        ] {
            assert!(body.contains(expected), "missing `{expected}` in:\n{body}");
        }

        // Histograms expose the full bucket set, +Inf, _sum and _count.
        for name in [
            "lobsterai_proxy_request_duration_seconds",
            "lobsterai_proxy_ttft_seconds",
            "lobsterai_proxy_first_reasoning_seconds",
            "lobsterai_proxy_first_text_seconds",
            "lobsterai_proxy_first_tool_seconds",
        ] {
            assert!(
                body.contains(&format!("{name}_bucket{{le=\"0.005\"}}")),
                "{name}"
            );
            assert!(
                body.contains(&format!("{name}_bucket{{le=\"+Inf\"}}")),
                "{name}"
            );
            assert!(body.contains(&format!("{name}_count")), "{name}");
            assert!(body.contains(&format!("# TYPE {name} histogram")), "{name}");
        }
        // One observation per histogram; the duration sum is the recorded 1.234s.
        assert!(body.contains("lobsterai_proxy_request_duration_seconds_count 1"));
        assert!(body.contains("lobsterai_proxy_request_duration_seconds_sum 1.234000"));
    }

    #[test]
    fn unobserved_latencies_are_not_counted_as_zero() {
        let metrics = Metrics::default();
        metrics.record_latencies(&RequestSummary {
            duration_ms: 100,
            ..RequestSummary::default()
        });
        let body = render(&metrics, ScrapeGauges::default(), crate::VERSION, 0);
        assert!(body.contains("lobsterai_proxy_request_duration_seconds_count 1"));
        // No TTFT was observed, so the TTFT histogram must have zero samples.
        assert!(body.contains("lobsterai_proxy_ttft_seconds_count 0"));
        assert!(body.contains("lobsterai_proxy_first_tool_seconds_count 0"));
    }

    /// The exposition must never carry credential-shaped or secret-adjacent
    /// strings, and must not embed the build's runtime state.
    #[test]
    fn render_contains_no_credentials_or_identifiers() {
        let metrics = fresh_metrics();
        let body = render(&metrics, ScrapeGauges::default(), crate::VERSION, 1);
        let lower = body.to_lowercase();
        for needle in [
            "access_token",
            "accesstoken",
            "refresh_token",
            "refreshtoken",
            "bearer",
            "authorization",
            "x-api-key",
            "api_key",
            "apikey",
            "secret",
            "uid",
            "session",
            "nickname",
            "password",
            "h.eyj", // JWT header prefix used by LobsterAI access tokens
        ] {
            assert!(
                !lower.contains(needle),
                "forbidden token `{needle}` leaked into /metrics:\n{body}"
            );
        }
    }

    #[test]
    fn in_flight_guard_balances_on_drop() {
        let metrics = std::sync::Arc::new(Metrics::default());
        {
            let _guard = InFlightGuard::new(metrics.clone(), true);
            assert_eq!(metrics.requests_in_flight.load(Ordering::Relaxed), 1);
            assert_eq!(metrics.streams_in_flight.load(Ordering::Relaxed), 1);
        }
        assert_eq!(metrics.requests_in_flight.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.streams_in_flight.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn histogram_buckets_are_cumulative_and_unbounded_observation_lands_in_inf() {
        let histogram = Histogram::default();
        histogram.observe_ms(3); // 0.003s �?first bucket
        histogram.observe_ms(20); // 0.02s �?second bucket
        histogram.observe_ms(60_000); // 60s �?above the last finite bound
        let body = render(
            &Metrics::default(),
            ScrapeGauges::default(),
            crate::VERSION,
            0,
        );
        assert!(body.contains("lobsterai_proxy_request_duration_seconds_bucket{le=\"+Inf\"} 0"));
        let cumulative = histogram.bucket_counts();
        assert_eq!(cumulative[0], 1, "0.005s bucket holds only the 3ms sample");
        // 20ms exceeds the 0.01s bound, so it lands in the 0.025s bucket; the
        // 0.005s bucket still accumulates it cumulatively.
        assert_eq!(cumulative[1], 1, "0.01s bucket is below the 20ms sample");
        assert_eq!(cumulative[2], 2, "0.025s bucket holds both small samples");
        assert_eq!(*cumulative.last().unwrap(), 2, "60s sample is unbucketed");
        assert_eq!(
            histogram.count(),
            3,
            "every sample counts, even beyond +Inf"
        );
    }
}
