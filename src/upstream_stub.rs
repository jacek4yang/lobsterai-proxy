//! Stub upstream failure type for the scaffold PR; replaced by the full
//! `lobsterai` module in the next PR.

/// A terminal upstream failure (already sanitized).
#[derive(Debug, Clone)]
pub struct ApiFailure {
    pub status: u16,
    pub kind: &'static str,
    pub message: String,
    pub retry_after_secs: Option<i64>,
    /// True when the failure occurred before any semantic output (retry-safe).
    pub before_output: bool,
}
