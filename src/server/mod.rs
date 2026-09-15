//! HTTP server module: request orchestration and the streaming pump
//! (watchdog + pings), non-stream aggregation. All retry invariants are
//! enforced before any byte reaches the client: once the stream is flowing
//! there is no replay, no failover, no regeneration.

mod orchestrator;
mod responses_pump;

pub use orchestrator::{build_state, print_status, router, serve, AppState};
