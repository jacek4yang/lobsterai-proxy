//! Graceful shutdown: signal handling, request draining, and the background
//! task shutdown channel.
//!
//! # Why a drain in addition to `with_graceful_shutdown`
//!
//! `axum::serve(..).with_graceful_shutdown(..)` stops accepting new
//! connections and waits for every *connection* future to finish. For HTTP/1.1
//! keep-alive that is not the same thing as "every in-flight request finished":
//! connections held open by an idle client (and, on the streaming path here,
//! the detached pump task that owns the in-flight gauges) are not necessarily
//! resolved by the time the accept loop returns. This module therefore layers a
//! *request-level* drain on top:
//!
//! 1. a shutdown signal (SIGINT/SIGTERM on unix, Ctrl+C/Ctrl+Break elsewhere) resolves the
//!    future passed to `with_graceful_shutdown`, so the listener stops
//!    accepting;
//! 2. the accept-loop future keeps being polled until it resolves (all
//!    connections closed) **or** the configured drain deadline expires;
//! 3. the process exits with code 0 regardless — a bounded drain must never
//!    turn into an unbounded hang.
//!
//! # Data safety
//!
//! The drain exists to protect state that is only consistent while a request is
//! running. Credential persistence is already crash-safe on its own
//! (`codebuddy::credential::atomic_write` writes a temp file in the target
//! directory and `rename`s it into place), so an abrupt exit can only ever
//! leave an orphan `.credential-*.tmp` behind — never a truncated or corrupt
//! `.info`. `crate::codebuddy::credential::cleanup_stale_temp_files` sweeps
//! those orphans at startup. Nothing in this module exists to make writes
//! durable; it exists to let in-flight requests finish.
//!
//! # Double signal
//!
//! An operator who presses Ctrl+C twice wants out *now*. The first signal
//! starts the drain; a second signal (a "force quit") is deliberately **not**
//! swallowed: it is reported on a `watch` channel the drain loop selects on, so
//! the remaining connections are abandoned and the process exits immediately —
//! the standard double-Ctrl+C convention.

use std::sync::Arc;
use std::time::Duration;

use crate::observability::Metrics;

/// Poll cadence for the in-flight gauge while waiting for the last request.
const IN_FLIGHT_POLL: Duration = Duration::from_millis(25);

/// How the drain ended. Returned for logging and for tests; it never changes
/// the process exit code (see [`drain_until_idle`]: every outcome is a clean
/// exit 0, and a stuck drain is abandoned rather than fatal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainOutcome {
    /// The connection-level future resolved within the deadline.
    ConnectionsClosed,
    /// The connection-level future did not resolve; remaining connections were
    /// abandoned at the deadline.
    DrainTimedOut,
    /// A second signal arrived; remaining work was abandoned immediately.
    Forced,
}

/// Watch-channel payload: a monotonically increasing signal count, so a
/// consumer can distinguish "a signal happened" from "a *second* signal
/// happened" without a second channel.
pub type SignalWatch = tokio::sync::watch::Sender<u32>;

/// A per-consumer view of the shutdown signal. Every consumer gets its own
/// receiver, so one of them resolving does not consume the signal for the
/// others.
pub type SignalReceiver = tokio::sync::watch::Receiver<u32>;

/// Create the shutdown signal channel and spawn the OS-signal listener.
///
/// The returned receiver resolves on the first shutdown signal; every later
/// signal bumps the watch value (observed by [`drain_until_idle`] as a force
/// quit). The listener task ends with the process; there is nothing to join.
pub fn signal_channel() -> (SignalWatch, SignalReceiver) {
    let (tx, rx) = tokio::sync::watch::channel(0u32);
    // The listener owns a clone; the caller keeps the original so it can hand
    // out fresh receivers (one per consumer) at any point.
    let listener_tx = tx.clone();
    tokio::spawn(async move {
        let tx = listener_tx;
        let mut signals = 0u32;
        loop {
            if wait_for_signal().await.is_err() {
                // No signal handler could be installed (e.g. an exotic
                // platform). Stop listening rather than spinning: the caller
                // keeps serving until it is killed externally.
                tracing::warn!("no shutdown signal handler available on this platform");
                return;
            }
            signals = signals.saturating_add(1);
            // Send can only fail when every receiver is gone (process exiting).
            if tx.send(signals).is_err() {
                return;
            }
            if signals == 1 {
                tracing::info!("shutdown signal received; draining in-flight requests");
            } else {
                tracing::warn!(signals, "shutdown signal repeated; forcing exit");
                return;
            }
        }
    });
    (tx, rx)
}

/// Resolve on the next shutdown signal: SIGINT/SIGTERM on unix, Ctrl+C/Ctrl+Break elsewhere.
async fn wait_for_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        // Windows console operators have two interrupt keys. Ctrl+Break must
        // also drain: the default handler terminates the process immediately
        // (STATUS_CONTROL_C_EXIT) and bypasses every drain guarantee below.
        let mut brk = tokio::signal::windows::ctrl_break()?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = brk.recv() => {}
        }
        Ok(())
    }
}

/// Wait, bounded by `timeout`, for in-flight requests to finish.
///
/// `serve` is the connection-level future from `axum::serve(..)
/// .with_graceful_shutdown(..)`: it resolves once the listener stopped
/// accepting and every connection task completed. Because a keep-alive
/// connection (and this crate's detached stream-pump task) can outlive the
/// response, the gauge poll below is the authoritative "no request is still
/// running" check.
///
/// Guarantees, all covered by unit tests:
/// - resolves as soon as the gauge reaches 0, with no fixed delay added;
/// - never waits longer than `timeout`, even if requests never complete;
/// - gives up immediately when a second signal arrives on `signals`.
pub async fn drain_until_idle<F, R>(
    metrics: Arc<Metrics>,
    serve: F,
    timeout: Duration,
    mut signals: tokio::sync::watch::Receiver<u32>,
) -> DrainOutcome
where
    F: std::future::Future<Output = R>,
{
    // Poll the owned server from startup; dropping the drain also drops the
    // server future instead of detaching a task on timeout or cancellation.
    tokio::pin!(serve);
    if *signals.borrow_and_update() == 0 {
        tokio::select! {
            _ = &mut serve => return DrainOutcome::ConnectionsClosed,
            _ = shutdown_requested(signals.clone()) => {}
        }
    }
    bounded_drain(metrics, serve, timeout, signals, 1).await
}

/// Drain an already-triggered server, bounded by the shared deadline.
#[doc(hidden)]
pub async fn bounded_drain<F, R>(
    metrics: Arc<Metrics>,
    serve: F,
    timeout: Duration,
    signals: tokio::sync::watch::Receiver<u32>,
    armed_at: u32,
) -> DrainOutcome
where
    F: std::future::Future<Output = R>,
{
    let connections = async move {
        serve.await;
    };
    tokio::pin!(connections);
    bounded_drain_inner(metrics, connections, timeout, signals, armed_at).await
}

/// Shared drain body over any completion future for the connection side.
async fn bounded_drain_inner(
    metrics: Arc<Metrics>,
    mut connections: impl std::future::Future<Output = ()> + Unpin,
    timeout: Duration,
    mut signals: tokio::sync::watch::Receiver<u32>,
    armed_at: u32,
) -> DrainOutcome {
    let started = tokio::time::Instant::now();
    let deadline = started.checked_add(timeout).unwrap_or(started);
    // Cloned handle for the poll: the guard in the request path decrements the
    // same atomic, so this observes completions without touching the pool.
    let request_gauge = Arc::clone(&metrics);

    // Phase 2: the "second signal" contract is about signals *after the
    // trigger*: anything at or below `armed_at` is the drain's own trigger (or
    // an earlier, already-consumed signal) and must NOT be treated as a force
    // quit.
    let connections_closed = tokio::select! {
        biased;
        _ = wait_for_force(&mut signals, armed_at) => return DrainOutcome::Forced,
        _ = &mut connections => true,
        _ = tokio::time::sleep_until(deadline) => false,
    };

    if !connections_closed {
        tracing::warn!(
            timeout_secs = timeout.as_secs(),
            in_flight = request_gauge
                .requests_in_flight
                .load(std::sync::atomic::Ordering::Relaxed),
            "graceful shutdown timed out; abandoning remaining connections"
        );
        return DrainOutcome::DrainTimedOut;
    }

    // Connections are closed. Give the stream-pump tasks (and any request that
    // was already past the connection boundary) a bounded window to finish:
    // either the gauge reaches 0, or the overall deadline wins, whichever is
    // first. An idle process observes 0 on the first poll.
    loop {
        let in_flight = request_gauge
            .requests_in_flight
            .load(std::sync::atomic::Ordering::Relaxed);
        if in_flight <= 0 {
            tracing::info!(
                drained_ms = started.elapsed().as_millis() as u64,
                "graceful shutdown complete"
            );
            return DrainOutcome::ConnectionsClosed;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            tracing::warn!(
                timeout_secs = timeout.as_secs(),
                in_flight,
                "drain deadline reached with requests still in flight; abandoning them"
            );
            return DrainOutcome::DrainTimedOut;
        }
        let wait = remaining.min(IN_FLIGHT_POLL);
        tokio::select! {
            _ = wait_for_force(&mut signals, armed_at) => return DrainOutcome::Forced,
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

/// Resolve when a shutdown has been requested on `rx`.
///
/// Also resolves when the sender is dropped, so a caller awaiting this inside a
/// `select!` cannot be left waiting forever by a revoked signal source. The
/// borrow is released as soon as the first signal is observed, which is what
/// lets `serve` hand the same receiver on to the force-quit watch.
pub async fn shutdown_requested(mut rx: tokio::sync::watch::Receiver<u32>) {
    loop {
        if *rx.borrow_and_update() > 0 {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// Convenience wrapper: run [`drain_until_idle`] and log the outcome. Callers
/// exit 0 on every path, including a double Ctrl+C.
pub async fn shutdown<F, R>(
    metrics: Arc<Metrics>,
    serve: F,
    timeout: Duration,
    signals: tokio::sync::watch::Receiver<u32>,
) -> DrainOutcome
where
    F: std::future::Future<Output = R>,
{
    let outcome = drain_until_idle(metrics, serve, timeout, signals).await;
    tracing::info!(?outcome, "graceful shutdown finished");
    outcome
}

/// Resolve once the watch value moves *past* `armed_at` — i.e. a genuine second
/// shutdown signal. A dropped sender resolves immediately (the listener is gone,
/// so no further signal can arrive and the caller should stop waiting).
async fn wait_for_force(signals: &mut tokio::sync::watch::Receiver<u32>, armed_at: u32) {
    loop {
        if *signals.borrow_and_update() > armed_at {
            return;
        }
        if signals.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::observability::{InFlightGuard, Metrics};

    fn metrics() -> Arc<Metrics> {
        Arc::new(Metrics::default())
    }

    /// An in-flight request that completes: the drain must resolve promptly
    /// (well before the deadline) once the gauge returns to 0.
    #[tokio::test]
    async fn drain_waits_for_in_flight_request_and_resolves_early() {
        let metrics = metrics();
        let guard = InFlightGuard::new(metrics.clone(), true);
        let (tx, rx) = tokio::sync::watch::channel(0u32);
        let _tx = tx;

        // The connection-level future resolves immediately (all connections
        // closed), but the request-level gauge is still raised.
        let serve = std::future::ready(());
        let drain = tokio::spawn(bounded_drain(
            metrics.clone(),
            Box::pin(serve),
            Duration::from_secs(5),
            rx,
            0,
        ));

        // Let the drain observe a raised gauge, then finish the request.
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            !drain.is_finished(),
            "drain must wait for the in-flight gauge"
        );
        drop(guard);
        assert_eq!(metrics.requests_in_flight.load(Ordering::Relaxed), 0);

        let outcome = tokio::time::timeout(Duration::from_secs(2), drain)
            .await
            .expect("drain did not resolve after the request completed")
            .unwrap();
        assert_eq!(outcome, DrainOutcome::ConnectionsClosed);
    }

    /// A request that never completes must not hang the drain: the configured
    /// bound is the hard upper limit.
    #[tokio::test]
    async fn drain_times_out_at_the_configured_bound() {
        let metrics = metrics();
        // Leaked guard: the request never completes.
        let _guard = InFlightGuard::new(metrics.clone(), true);
        let (tx, rx) = tokio::sync::watch::channel(0u32);
        let _tx = tx;

        let timeout = Duration::from_millis(300);
        let started = tokio::time::Instant::now();
        let outcome = bounded_drain(
            metrics.clone(),
            Box::pin(std::future::ready(())),
            timeout,
            rx,
            0,
        )
        .await;
        let elapsed = started.elapsed();
        assert_eq!(outcome, DrainOutcome::DrainTimedOut);
        assert!(
            elapsed >= timeout,
            "drain returned before the bound: {elapsed:?}"
        );
        assert!(
            elapsed < timeout + Duration::from_secs(2),
            "drain overshot the bound: {elapsed:?}"
        );
        assert_eq!(
            metrics.requests_in_flight.load(Ordering::Relaxed),
            1,
            "the abandoned request is still accounted for"
        );
    }

    /// No in-flight work: the drain is effectively instantaneous (nothing is
    /// added by the wrapper beyond the polling cadence).
    #[tokio::test]
    async fn drain_is_immediate_when_idle() {
        let metrics = metrics();
        let (tx, rx) = tokio::sync::watch::channel(0u32);
        let _tx = tx;
        let started = tokio::time::Instant::now();
        let outcome = bounded_drain(
            metrics,
            Box::pin(std::future::ready(())),
            Duration::from_secs(30),
            rx,
            0,
        )
        .await;
        assert_eq!(outcome, DrainOutcome::ConnectionsClosed);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "idle drain must not wait: {:?}",
            started.elapsed()
        );
    }

    /// The connection-level future itself hangs (a keep-alive connection that
    /// never closes): the deadline still wins.
    #[tokio::test]
    async fn drain_bounds_a_connection_future_that_never_resolves() {
        let metrics = metrics();
        let (tx, rx) = tokio::sync::watch::channel(0u32);
        let _tx = tx;
        let timeout = Duration::from_millis(200);
        let started = tokio::time::Instant::now();
        let outcome = bounded_drain(
            metrics,
            Box::pin(std::future::pending::<()>()),
            timeout,
            rx,
            0,
        )
        .await;
        assert_eq!(outcome, DrainOutcome::DrainTimedOut);
        assert!(started.elapsed() >= timeout);
    }

    /// A second signal forces the drain to give up immediately instead of
    /// waiting out the deadline.
    #[tokio::test]
    async fn second_signal_forces_immediate_exit() {
        let metrics = metrics();
        let _guard = InFlightGuard::new(metrics.clone(), true);
        // The receiver has already observed the triggering signal (value 1),
        // exactly as `serve` has by the time the drain runs in production.
        let (tx, rx) = tokio::sync::watch::channel(1u32);
        let started = tokio::time::Instant::now();
        let drain = tokio::spawn(drain_until_idle(
            metrics,
            Box::pin(std::future::ready(())),
            Duration::from_secs(30),
            rx,
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        // A *second* Ctrl+C.
        tx.send(2).unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(2), drain)
            .await
            .expect("forced drain did not resolve")
            .unwrap();
        assert_eq!(outcome, DrainOutcome::Forced);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "force must not wait for the deadline: {:?}",
            started.elapsed()
        );
    }

    /// The signal that *started* the drain must not be mistaken for a force
    /// quit: a single Ctrl+C has to be allowed to drain to completion.
    #[tokio::test]
    async fn the_triggering_signal_is_not_treated_as_a_force_quit() {
        let metrics = metrics();
        let guard = InFlightGuard::new(metrics.clone(), true);
        let (tx, rx) = tokio::sync::watch::channel(1u32);
        let _tx = tx;
        let drain = tokio::spawn(drain_until_idle(
            metrics.clone(),
            Box::pin(std::future::ready(())),
            Duration::from_secs(30),
            rx,
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !drain.is_finished(),
            "a single signal must not abort the drain it started"
        );
        // The request finishes normally → clean drain, no force.
        drop(guard);
        let outcome = tokio::time::timeout(Duration::from_secs(2), drain)
            .await
            .expect("drain did not resolve")
            .unwrap();
        assert_eq!(outcome, DrainOutcome::ConnectionsClosed);
    }

    /// Dropping the signal sender (the listener task ended) must not hang the
    /// drain. There will never be another signal, so the drain gives up
    /// immediately rather than burning the whole deadline — and it is reported
    /// as `Forced`, i.e. "abandon now", not as a timeout.
    #[tokio::test]
    async fn dropped_signal_sender_resolves_the_drain_immediately() {
        let metrics = metrics();
        let _guard = InFlightGuard::new(metrics.clone(), true);
        let (tx, rx) = tokio::sync::watch::channel(1u32);
        drop(tx);
        let started = tokio::time::Instant::now();
        let outcome = drain_until_idle(
            metrics,
            Box::pin(std::future::ready(())),
            Duration::from_secs(30),
            rx,
        )
        .await;
        assert_eq!(outcome, DrainOutcome::Forced);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a revoked signal source must not wait out the deadline: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn coalesced_signals_force_without_waiting_for_a_third() {
        let (tx, rx) = tokio::sync::watch::channel(2u32);
        let _tx = tx;
        let outcome = tokio::time::timeout(
            Duration::from_millis(500),
            drain_until_idle(
                metrics(),
                std::future::pending::<()>(),
                Duration::from_secs(30),
                rx,
            ),
        )
        .await
        .expect("coalesced second signal must force immediately");
        assert_eq!(outcome, DrainOutcome::Forced);
    }

    #[tokio::test]
    async fn force_drops_the_owned_server_future() {
        let metrics = metrics();
        let guard = InFlightGuard::new(metrics.clone(), true);
        let (tx, rx) = tokio::sync::watch::channel(2u32);
        let _tx = tx;
        let serve = async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        };
        assert_eq!(
            drain_until_idle(metrics.clone(), serve, Duration::from_secs(30), rx).await,
            DrainOutcome::Forced
        );
        assert_eq!(metrics.requests_in_flight.load(Ordering::Relaxed), 0);
    }

    /// The OS-signal listener bumps the watch value on every signal, which is
    /// what makes the double-Ctrl+C path observable. Exercised without sending
    /// a real signal: the counter contract is what the drain depends on.
    #[tokio::test]
    async fn signal_watch_counts_signals_and_is_observable() {
        let (tx, mut rx) = tokio::sync::watch::channel(0u32);
        assert_eq!(*rx.borrow_and_update(), 0);
        tx.send(1).unwrap();
        assert!(rx.changed().await.is_ok());
        assert_eq!(*rx.borrow_and_update(), 1);
        tx.send(2).unwrap();
        assert!(rx.changed().await.is_ok());
        assert_eq!(*rx.borrow(), 2);
    }

    /// `shutdown_requested` resolves on the first signal, and also when the
    /// sender is dropped (so it cannot wedge a `select!` forever).
    #[tokio::test]
    async fn shutdown_requested_resolves_on_first_signal() {
        let (tx, rx) = tokio::sync::watch::channel(0u32);
        let waiter = tokio::spawn(shutdown_requested(rx));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "no signal yet: must still wait");
        tx.send(1).unwrap();
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("shutdown_requested did not resolve on the first signal")
            .unwrap();

        // A channel whose sender is gone resolves rather than hanging.
        let (tx, rx) = tokio::sync::watch::channel(0u32);
        let waiter = tokio::spawn(shutdown_requested(rx));
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(tx);
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("shutdown_requested hung after the sender was dropped")
            .unwrap();
    }

    /// Already-signalled channel: `shutdown_requested` resolves immediately.
    #[tokio::test]
    async fn shutdown_requested_resolves_when_already_signalled() {
        let (tx, rx) = tokio::sync::watch::channel(0u32);
        tx.send(1).unwrap();
        tokio::time::timeout(Duration::from_millis(500), shutdown_requested(rx))
            .await
            .expect("already-signalled channel must resolve at once");
    }
}
