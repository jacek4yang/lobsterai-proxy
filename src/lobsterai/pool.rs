//! Placeholder account pool for the scaffold PR; the full multi-account
//! pool arrives in the next PR.

/// Account pool stub.
#[derive(Default)]
pub struct Pool;

impl Pool {
    /// Safe snapshot for /admin/status: names, health, cooldowns; no secrets.
    pub fn snapshot(&self, _model: &str) -> Vec<serde_json::Value> {
        Vec::new()
    }
}
