//! Background housekeeping:
//! - every minute: rescan the auth directory, refresh credentials near expiry
//!   or due for daily keepalive;
//! - on the check-in schedule (default hourly, first run shortly after start):
//!   daily check-in per account (idempotent per day upstream) + credit
//!   refresh, feeding the highest-credits-first pool selection.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use super::pool::Pool;

/// A shutdown handle for a background task that must stop cleanly.
pub type ShutdownReceiver = watch::Receiver<u32>;

/// Send `stop` atomically.
pub fn shutdown_channel() -> (watch::Sender<u32>, ShutdownReceiver) {
    watch::channel(0u32)
}

/// Resolve when a shutdown has been requested on `rx`.
pub async fn shutdown_requested(rx: &mut watch::Receiver<u32>) {
    loop {
        if *rx.borrow_and_update() > 0 {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// Spawn the background loops. `first_checkin_delay` is ~30s in production.
///
/// The loop only ever exits *between* iterations: the shutdown signal is one
/// arm of the `select!`, so a pass is never cancelled halfway.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    pool: Arc<Pool>,
    http: reqwest::Client,
    base_url: String,
    auth_dir: PathBuf,
    margin_secs: i64,
    keepalive_secs: u64,
    checkin: crate::config::CheckinConfig,
    first_checkin_delay: Duration,
    metrics: Arc<crate::observability::Metrics>,
    mut shutdown: ShutdownReceiver,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut refresh_tick = tokio::time::interval(Duration::from_secs(60));
        refresh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        refresh_tick.reset();
        let mut checkin_tick =
            tokio::time::interval(Duration::from_secs(checkin.interval_secs.max(60)));
        checkin_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        checkin_tick.reset();
        tokio::select! {
            _ = tokio::time::sleep(first_checkin_delay) => {}
            _ = shutdown_requested(&mut shutdown) => return,
        }
        // First housekeeping pass shortly after startup, then on schedule.
        if checkin.enabled {
            checkin_all(&pool, &http, &base_url).await;
        }
        loop {
            tokio::select! {
                _ = shutdown_requested(&mut shutdown) => {
                    tracing::info!("housekeeping stopping (shutdown requested)");
                    return;
                }
                _ = refresh_tick.tick() => {
                    pool.prune();
                    let files = super::pool::list_credential_files(&auth_dir);
                    pool.load_files(&files);
                    pool.refresh_due(&http, &base_url, margin_secs, keepalive_secs, Some(&metrics))
                        .await;
                }
                _ = checkin_tick.tick() => {
                    if checkin.enabled {
                        checkin_all(&pool, &http, &base_url).await;
                    }
                }
            }
        }
    })
}

use super::checkin::checkin_all;
