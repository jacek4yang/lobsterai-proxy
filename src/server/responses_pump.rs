//! Responses streaming pump and non-stream aggregation. Mirrors the
//! Anthropic pump in `orchestrator.rs`: same watchdog, ping, no-replay
//! discipline; only the downstream event rendering differs.

use super::orchestrator::{
    stall_error, trim_ascii, AppState, InFlightGuard, PumpItem, StreamStats, MAX_SSE_LINE_BYTES,
    PING_INTERVAL,
};
use crate::error::ApiError;
use crate::observability::{log_request_summary, Metrics, RequestSummary};
use crate::reasoning_shadow::ReasoningShadowStore;
use crate::responses::stream::ResponsesConverter;
use crate::stream_watch::StreamTimeouts;
use axum::body::Bytes;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Instant;

/// Response-side stream pump: consumes upstream chat SSE into a Responses SSE
/// stream, with the same watchdog/ping/no-replay discipline as the Anthropic
/// pump. Emits an SSE comment keepalive (`: ping`) - never a synthetic event
/// frame, which would fail the client's typed deserialization.
#[allow(clippy::too_many_arguments)]
pub(crate) fn responses_stream_response(
    state: &AppState,
    model: String,
    expose_thinking: bool,
    session_fp: Option<String>,
    started_at: Instant,
    summary: &mut RequestSummary,
    in_flight: InFlightGuard,
    response: reqwest::Response,
    metrics: Arc<Metrics>,
) -> Response {
    let timeouts = state.timeouts;
    let (tx, rx) = tokio::sync::mpsc::channel::<PumpItem>(64);
    let (stats_tx, stats_rx) = tokio::sync::oneshot::channel::<(StreamStats, Option<String>)>();
    let shadow = state.shadow.clone();
    let response_stream = response.bytes_stream();
    let (pump_done_tx, pump_done) = tokio::sync::oneshot::channel::<()>();
    let close_watch = tx.clone();
    let pump_metrics = metrics.clone();

    let _pump_task = tokio::spawn(async move {
        let metrics = pump_metrics;
        let _in_flight = in_flight;
        let mut converter = ResponsesConverter::new(&model, expose_thinking);
        let mut watch =
            crate::stream_watch::StreamWatch::new(timeouts, tokio::time::Instant::now());
        let mut buffer: Vec<u8> = Vec::with_capacity(8192);
        let mut stats = StreamStats::default();
        let mut byte_stream = response_stream;
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut out: Vec<u8> = Vec::with_capacity(4096);
        let mut terminal: Option<(StreamStats, Option<String>)> = None;
        let mut done = false;

        while !done {
            tokio::select! {
                biased;
                _ = close_watch.closed() => {
                    // Client went away mid-stream: stop consuming upstream.
                    let _ = pump_done_tx.send(());
                    return;
                }
                _ = ping.tick() => {
                    out.extend_from_slice(crate::responses::types::sse_ping().as_bytes());
                }
                _ = tokio::time::sleep_until(watch.next_deadline()) => {
                    if let Some(stall) = watch.check(tokio::time::Instant::now()) {
                        metrics.record_stream_stall(stall);
                        let message = stall_error(stall);
                        finish(&mut converter, &mut stats, Some(message.clone()), &mut out, &shadow, &session_fp);
                        terminal = Some((stats.clone(), Some(message)));
                        done = true;
                    }
                }
                chunk = byte_stream.next() => {
                    match chunk {
                        None => {
                            let error = (converter.finish_reason().is_none())
                                .then(|| "upstream stream ended unexpectedly".to_owned());
                            finish(&mut converter, &mut stats, error.clone(), &mut out, &shadow, &session_fp);
                            terminal = Some((stats.clone(), error));
                            done = true;
                        }
                        Some(Err(err)) => {
                            let message = format!("upstream stream error: {err}");
                            finish(&mut converter, &mut stats, Some(message.clone()), &mut out, &shadow, &session_fp);
                            terminal = Some((stats.clone(), Some(message)));
                            done = true;
                        }
                        Some(Ok(bytes)) => {
                            watch.on_upstream_bytes(tokio::time::Instant::now());
                            buffer.extend_from_slice(&bytes);
                            while let Some(newline) = buffer.iter().position(|&b| b == b'\n') {
                                let line: Vec<u8> = buffer.drain(..=newline).collect();
                                let line = trim_ascii(&line[..line.len() - 1]);
                                if line.is_empty() {
                                    continue;
                                }
                                if line.len() > MAX_SSE_LINE_BYTES {
                                    continue;
                                }
                                watch.on_sse_event();
                                let Some(payload) = line.strip_prefix(b"data:") else {
                                    continue;
                                };
                                let payload = trim_ascii(payload);
                                if payload == b"[DONE]" {
                                    let error = (converter.finish_reason().is_none())
                                        .then(|| "upstream stream ended unexpectedly".to_owned());
                                    finish(&mut converter, &mut stats, error.clone(), &mut out, &shadow, &session_fp);
                                    terminal = Some((stats.clone(), error));
                                    done = true;
                                    break;
                                }
                                let Ok(chunk_value) = serde_json::from_slice::<serde_json::Value>(payload) else {
                                    continue;
                                };
                                let semantic = converter.feed_chunk(&chunk_value, &mut out);
                                if semantic {
                                    let elapsed = started_at.elapsed().as_millis();
                                    watch.on_semantic(tokio::time::Instant::now());
                                    if stats.ttft_ms.is_none() {
                                        stats.ttft_ms = Some(elapsed);
                                    }
                                    if stats.first_reasoning_ms.is_none() && converter.has_reasoning() {
                                        stats.first_reasoning_ms = Some(elapsed);
                                    }
                                    if stats.first_text_ms.is_none() && converter.has_text() {
                                        stats.first_text_ms = Some(elapsed);
                                    }
                                    if stats.first_tool_ms.is_none() && converter.has_tools() {
                                        stats.first_tool_ms = Some(elapsed);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if !out.is_empty() {
                let payload = std::mem::take(&mut out);
                if tx.send(PumpItem::Bytes(payload)).await.is_err() {
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

    let mut summary = std::mem::take(summary);
    let _summary_task = tokio::spawn(async move {
        let (stats, error) = match pump_done.await {
            Ok(()) => match stats_rx.await {
                Ok(result) => result,
                Err(_) => return,
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

/// Shadow bookkeeping (same rule as the Anthropic pump).
fn apply_shadow_bookkeeping(
    converter: &ResponsesConverter,
    shadow: &ReasoningShadowStore,
    session_fp: &Option<String>,
) {
    if let Some(fp) = session_fp {
        let ids = converter.tool_call_ids();
        if converter.finish_reason() == Some("tool_calls") {
            shadow.store(fp, &ids, converter.reasoning_text());
        } else if converter.finish_reason().is_some() {
            shadow.clear_session(fp);
        }
    }
}

/// Shared terminal bookkeeping for the aggregated path.
fn finish(
    converter: &mut ResponsesConverter,
    stats: &mut StreamStats,
    error: Option<String>,
    out: &mut Vec<u8>,
    shadow: &ReasoningShadowStore,
    session_fp: &Option<String>,
) {
    converter.finish(error.as_deref(), out);
    stats.finish_reason = converter.finish_reason().map(str::to_owned);
    if let Some((input, output, cached, _reasoning)) = converter.usage() {
        stats.input_tokens = Some(input);
        stats.output_tokens = Some(output);
        stats.cached_tokens = Some(cached);
    }
    stats.text_bytes = converter.text_bytes();
    stats.reasoning_bytes = converter.reasoning_bytes();
    stats.tool_bytes = converter.tool_bytes();
    apply_shadow_bookkeeping(converter, shadow, session_fp);
}

/// Non-stream Responses: ONE upstream stream, aggregated locally into a
/// single Responses object by the same converter the streaming path uses.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn aggregate_responses(
    model: String,
    expose_thinking: bool,
    timeouts: StreamTimeouts,
    shadow: Arc<ReasoningShadowStore>,
    session_fp: Option<String>,
    started_at: Instant,
    summary: &mut RequestSummary,
    metrics: &Metrics,
    response: reqwest::Response,
) -> Result<serde_json::Value, ApiError> {
    let mut converter = ResponsesConverter::new(&model, expose_thinking);
    let mut watch = crate::stream_watch::StreamWatch::new(timeouts, tokio::time::Instant::now());
    let mut buffer: Vec<u8> = Vec::with_capacity(8192);
    let mut byte_stream = response.bytes_stream();
    let mut out: Vec<u8> = Vec::with_capacity(4096);
    loop {
        let deadline = watch.next_deadline();
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                if let Some(stall) = watch.check(tokio::time::Instant::now()) {
                    metrics.record_stream_stall(stall);
                    let message = stall_error(stall);
                    copy_responses_stats(&converter, started_at, summary);
                    return Err(ApiError::upstream(504, message));
                }
            }
            chunk = byte_stream.next() => {
                match chunk {
                    None => {
                        let error = (converter.finish_reason().is_none())
                            .then(|| "upstream stream ended unexpectedly".to_owned());
                        if let Some(error) = &error {
                            copy_responses_stats(&converter, started_at, summary);
                            return Err(ApiError::upstream(502, error.clone()));
                        }
                        let mut sink = Vec::new();
                        finish(&mut converter, &mut StreamStats::default(), None, &mut sink, &shadow, &session_fp);
                        copy_responses_stats(&converter, started_at, summary);
                        return Ok(converter.nonstream_response());
                    }
                    Some(Err(err)) => {
                        let message = format!("upstream stream error: {err}");
                        copy_responses_stats(&converter, started_at, summary);
                        return Err(ApiError::upstream(502, message));
                    }
                    Some(Ok(bytes)) => {
                        watch.on_upstream_bytes(tokio::time::Instant::now());
                        buffer.extend_from_slice(&bytes);
                        while let Some(newline) = buffer.iter().position(|&b| b == b'\n') {
                            let line: Vec<u8> = buffer.drain(..=newline).collect();
                            let line = trim_ascii(&line[..line.len() - 1]);
                            if line.is_empty() {
                                continue;
                            }
                            if line.len() > MAX_SSE_LINE_BYTES {
                                continue;
                            }
                            watch.on_sse_event();
                            let Some(payload) = line.strip_prefix(b"data:") else {
                                continue;
                            };
                            let payload = trim_ascii(payload);
                            if payload == b"[DONE]" {
                                let mut sink = Vec::new();
                                finish(&mut converter, &mut StreamStats::default(), None, &mut sink, &shadow, &session_fp);
                                copy_responses_stats(&converter, started_at, summary);
                                return Ok(converter.nonstream_response());
                            }
                            let Ok(chunk_value) = serde_json::from_slice::<serde_json::Value>(payload) else {
                                continue;
                            };
                            let semantic = converter.feed_chunk(&chunk_value, &mut out);
                            if semantic {
                                watch.on_semantic(tokio::time::Instant::now());
                                if summary.ttft_ms.is_none() {
                                    summary.ttft_ms = Some(started_at.elapsed().as_millis());
                                }
                                if summary.first_text_ms.is_none() && converter.has_text() {
                                    summary.first_text_ms = Some(started_at.elapsed().as_millis());
                                }
                                if summary.first_tool_ms.is_none() && converter.has_tools() {
                                    summary.first_tool_ms = Some(started_at.elapsed().as_millis());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Copy converter state into the request summary (aggregate path).
fn copy_responses_stats(
    converter: &ResponsesConverter,
    started_at: Instant,
    summary: &mut RequestSummary,
) {
    summary.duration_ms = started_at.elapsed().as_millis();
    summary.finish_reason = converter.finish_reason().map(str::to_owned);
    if let Some((input, output, cached, _reasoning)) = converter.usage() {
        summary.input_tokens = Some(input);
        summary.output_tokens = Some(output);
        summary.cached_tokens = Some(cached);
    }
    summary.text_bytes = converter.text_bytes();
    summary.reasoning_bytes = converter.reasoning_bytes();
    summary.tool_bytes = converter.tool_bytes();
}
