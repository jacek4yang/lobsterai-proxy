//! deepseek-flash specific policy: the wire behavior of this model
//! (reasoning via `reasoning_content`, automatic reasoning, no special
//! sampling fields) and the lossless request normalizations that keep
//! prompt prefixes stable across Claude Code turns.

pub mod policy;
pub mod reasoning;

/// The single model this build is optimized for.
pub const DEFAULT_MODEL: &str = "deepseek-flash";

/// Known-safe passthrough of Anthropic sampling params (no invented fields:
/// anything not observed in upstream behavior is not sent).
pub const SUPPORTED_SAMPLING_KEYS: [&str; 5] =
    ["temperature", "top_p", "top_k", "stop", "max_tokens"];
