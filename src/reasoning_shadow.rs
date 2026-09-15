//! Bounded, memory-only reasoning shadow store.
//!
//! When the client did NOT request thinking, deepseek-v4.1-flash reasoning
//! for a tool loop is consumed in-turn and never surfaced to Claude Code.
//! That keeps Claude Code history free of internal reasoning, but discards
//! the reasoning continuity the model benefits from across a multi-step
//! tool loop.
//!
//! The shadow store bridges that inside the proxy: when a response carries
//! reasoning + tool calls, the reasoning is kept here keyed by (session
//! fingerprint, tool-call id). The next request in the SAME reasoning epoch
//! that supplies a matching tool result gets the reasoning restored onto its
//! assistant message as `reasoning_content`.
//!
//! Hard resource rules:
//! - memory-only; never persisted, never logged (content or sizes beyond
//!   aggregate counters);
//! - disabled entirely when no stable session identity exists;
//! - bounded by session count, total bytes, per-entry bytes, and TTL;
//!   oversized reasoning is skipped (never truncated — truncated reasoning
//!   would be wrong context presented as real);
//! - entries evicted on final answer, TTL expiry, or LRU pressure; a restart
//!   loses everything by design.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One shadowed reasoning blob, shared by every tool call of the same
/// assistant turn.
struct ShadowEntry {
    reasoning: Arc<str>,
    created: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ShadowKey {
    session: [u8; 8],
    tool_call: u64,
}

/// Aggregate counters (sizes/counts only, safe for observability).
#[derive(Default)]
pub struct ShadowMetrics {
    pub stores: AtomicU64,
    pub restores: AtomicU64,
    pub misses: AtomicU64,
    pub evicted_entries: AtomicU64,
    pub dropped_oversized: AtomicU64,
    pub active_entries: AtomicU64,
}

#[derive(Debug, Clone, Copy)]
pub struct ShadowLimits {
    pub max_sessions: usize,
    pub max_total_bytes: usize,
    pub max_entry_bytes: usize,
    pub ttl: Duration,
}

impl Default for ShadowLimits {
    fn default() -> Self {
        Self {
            max_sessions: 256,
            max_total_bytes: 64 * 1024 * 1024,
            max_entry_bytes: 1024 * 1024,
            ttl: Duration::from_secs(600),
        }
    }
}

#[derive(Default)]
struct SessionState {
    entries: HashMap<ShadowKey, ShadowEntry>,
    bytes: usize,
}

pub struct ReasoningShadowStore {
    inner: Mutex<ShadowInner>,
    limits: ShadowLimits,
    pub metrics: ShadowMetrics,
}

struct ShadowInner {
    sessions: HashMap<String, SessionState>,
    lru: VecDeque<String>,
    total_bytes: usize,
}

impl ReasoningShadowStore {
    pub fn new(limits: ShadowLimits) -> Self {
        Self {
            inner: Mutex::new(ShadowInner {
                sessions: HashMap::new(),
                lru: VecDeque::new(),
                total_bytes: 0,
            }),
            limits,
            metrics: ShadowMetrics::default(),
        }
    }

    fn session_key(session_fingerprint: &str) -> [u8; 8] {
        let mut key = [0u8; 8];
        for (index, byte) in session_fingerprint.as_bytes().chunks(2).take(8).enumerate() {
            key[index] = u8::from_str_radix(std::str::from_utf8(byte).unwrap_or("0"), 16)
                .unwrap_or(index as u8);
        }
        key
    }

    /// Store reasoning for a completed assistant turn that issued tool calls;
    /// one blob shared by that turn's call ids.
    pub fn store(&self, session_fingerprint: &str, tool_call_ids: &[String], reasoning: &str) {
        if reasoning.is_empty() || tool_call_ids.is_empty() {
            return;
        }
        if reasoning.len() > self.limits.max_entry_bytes {
            self.metrics
                .dropped_oversized
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        let key_session = Self::session_key(session_fingerprint);
        let reasoning: Arc<str> = Arc::from(reasoning);
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        let is_new = !inner.sessions.contains_key(session_fingerprint);
        if is_new {
            inner.lru.push_back(session_fingerprint.to_owned());
        }
        let session = inner
            .sessions
            .entry(session_fingerprint.to_owned())
            .or_default();
        let added = reasoning.len().saturating_mul(tool_call_ids.len());
        for id in tool_call_ids {
            let mut hasher = DefaultHasher::new();
            id.hash(&mut hasher);
            session.entries.insert(
                ShadowKey {
                    session: key_session,
                    tool_call: hasher.finish(),
                },
                ShadowEntry {
                    reasoning: reasoning.clone(),
                    created: now,
                },
            );
        }
        session.bytes = session.bytes.saturating_add(added);
        inner.total_bytes = inner.total_bytes.saturating_add(added);
        self.metrics.stores.fetch_add(1, Ordering::Relaxed);
        self.enforce_limits(&mut inner);
        self.metrics
            .active_entries
            .store(self.entry_count(&inner), Ordering::Relaxed);
    }

    /// Restore shadowed reasoning for a historical assistant tool-call turn.
    pub fn restore(&self, session_fingerprint: &str, tool_call_ids: &[String]) -> Option<Arc<str>> {
        if tool_call_ids.is_empty() {
            return None;
        }
        let key_session = Self::session_key(session_fingerprint);
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        let session = inner.sessions.get_mut(session_fingerprint)?;
        for id in tool_call_ids {
            let mut hasher = DefaultHasher::new();
            id.hash(&mut hasher);
            let key = ShadowKey {
                session: key_session,
                tool_call: hasher.finish(),
            };
            if let Some(entry) = session.entries.get(&key) {
                if now.duration_since(entry.created) > self.limits.ttl {
                    session.entries.remove(&key);
                    self.metrics.misses.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
                self.metrics.restores.fetch_add(1, Ordering::Relaxed);
                return Some(entry.reasoning.clone());
            }
        }
        self.metrics.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Drop every entry for a session: final answer or shutdown.
    pub fn clear_session(&self, session_fingerprint: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(state) = inner.sessions.remove(session_fingerprint) {
            inner.total_bytes = inner.total_bytes.saturating_sub(state.bytes);
            self.metrics
                .evicted_entries
                .fetch_add(state.entries.len() as u64, Ordering::Relaxed);
            inner.lru.retain(|name| name != session_fingerprint);
        }
    }

    /// Restore shadowed reasoning into an OpenAI body in place: for every
    /// assistant message after the newest human user message that carries
    /// tool_calls but no `reasoning_content`, attach the shadowed reasoning.
    /// Misses are silent. Call BEFORE the historical-reasoning strip.
    pub fn restore_into(
        &self,
        body: &mut serde_json::Map<String, serde_json::Value>,
        session_fingerprint: &str,
    ) {
        let Some(messages) = body
            .get_mut("messages")
            .and_then(serde_json::Value::as_array_mut)
        else {
            return;
        };
        let epoch_boundary = crate::deepseek::policy::reasoning_epoch_boundary(messages);
        for message in messages.iter_mut().skip(epoch_boundary) {
            if message.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
                continue;
            }
            if message.get("reasoning_content").is_some() {
                continue;
            }
            let Some(calls) = message
                .get("tool_calls")
                .and_then(serde_json::Value::as_array)
            else {
                continue;
            };
            let ids: Vec<String> = calls
                .iter()
                .filter_map(|call| {
                    call.get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .collect();
            if let Some(reasoning) = self.restore(session_fingerprint, &ids) {
                message["reasoning_content"] = serde_json::Value::String((*reasoning).to_owned());
            }
        }
    }

    fn enforce_limits(&self, inner: &mut ShadowInner) {
        let now = Instant::now();
        let expired: Vec<String> = inner
            .sessions
            .iter()
            .filter(|(_, state)| {
                state
                    .entries
                    .values()
                    .all(|entry| now.duration_since(entry.created) > self.limits.ttl)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in expired {
            self.remove_session(inner, &name);
        }
        while inner.total_bytes > self.limits.max_total_bytes {
            let Some(victim) = inner.lru.front().cloned() else {
                break;
            };
            self.remove_session(inner, &victim);
        }
        while inner.sessions.len() > self.limits.max_sessions {
            let Some(victim) = inner.lru.front().cloned() else {
                break;
            };
            self.remove_session(inner, &victim);
        }
    }

    fn remove_session(&self, inner: &mut ShadowInner, name: &str) {
        if let Some(state) = inner.sessions.remove(name) {
            inner.total_bytes = inner.total_bytes.saturating_sub(state.bytes);
            self.metrics
                .evicted_entries
                .fetch_add(state.entries.len() as u64, Ordering::Relaxed);
        }
        inner.lru.retain(|candidate| candidate != name);
    }

    fn entry_count(&self, inner: &ShadowInner) -> u64 {
        inner
            .sessions
            .values()
            .map(|s| s.entries.len() as u64)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> ReasoningShadowStore {
        ReasoningShadowStore::new(ShadowLimits::default())
    }

    #[test]
    fn store_and_restore_roundtrip_shares_one_blob() {
        let shadow = store();
        shadow.store(
            "session_a",
            &["call_1".into(), "call_2".into()],
            "reasoning about the fix",
        );
        let restored_1 = shadow.restore("session_a", &["call_1".into()]).unwrap();
        let restored_2 = shadow.restore("session_a", &["call_2".into()]).unwrap();
        assert!(Arc::ptr_eq(&restored_1, &restored_2));
        assert_eq!(&*restored_1, "reasoning about the fix");
    }

    #[test]
    fn sessions_are_isolated() {
        let shadow = store();
        shadow.store("agent_a", &["call_1".into()], "A reasoning");
        shadow.store("agent_b", &["call_1".into()], "B reasoning");
        assert_eq!(
            &*shadow.restore("agent_a", &["call_1".into()]).unwrap(),
            "A reasoning"
        );
        assert_eq!(
            &*shadow.restore("agent_b", &["call_1".into()]).unwrap(),
            "B reasoning"
        );
        assert!(shadow.restore("agent_c", &["call_1".into()]).is_none());
    }

    #[test]
    fn clear_session_drops_everything() {
        let shadow = store();
        shadow.store("s", &["call_1".into()], "r");
        shadow.clear_session("s");
        assert!(shadow.restore("s", &["call_1".into()]).is_none());
    }

    #[test]
    fn oversized_reasoning_is_dropped_not_truncated() {
        let shadow = ReasoningShadowStore::new(ShadowLimits {
            max_entry_bytes: 1000,
            ..ShadowLimits::default()
        });
        shadow.store("s", &["call_1".into()], &"x".repeat(2000));
        assert!(shadow.restore("s", &["call_1".into()]).is_none());
        assert_eq!(shadow.metrics.dropped_oversized.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn total_byte_limit_evicts_lru_sessions() {
        let shadow = ReasoningShadowStore::new(ShadowLimits {
            max_sessions: 16,
            max_total_bytes: 2000,
            max_entry_bytes: 1000,
            ttl: Duration::from_secs(600),
        });
        let blob = "x".repeat(900);
        shadow.store("first", &["call_1".into()], &blob);
        shadow.store("second", &["call_1".into()], &blob);
        shadow.store("third", &["call_1".into()], &blob);
        assert!(shadow.restore("first", &["call_1".into()]).is_none());
        assert!(shadow.restore("third", &["call_1".into()]).is_some());
    }

    #[test]
    fn session_count_limit_is_enforced() {
        let shadow = ReasoningShadowStore::new(ShadowLimits {
            max_sessions: 2,
            ..ShadowLimits::default()
        });
        shadow.store("s1", &["call_1".into()], "r1");
        shadow.store("s2", &["call_1".into()], "r2");
        shadow.store("s3", &["call_1".into()], "r3");
        assert!(shadow.restore("s1", &["call_1".into()]).is_none());
        assert!(shadow.restore("s3", &["call_1".into()]).is_some());
    }

    #[test]
    fn expired_entries_are_lazily_dropped() {
        let shadow = ReasoningShadowStore::new(ShadowLimits {
            ttl: Duration::from_millis(1),
            ..ShadowLimits::default()
        });
        shadow.store("s", &["call_1".into()], "r");
        std::thread::sleep(Duration::from_millis(5));
        assert!(shadow.restore("s", &["call_1".into()]).is_none());
    }

    #[test]
    fn restore_into_attaches_only_in_current_epoch() {
        let shadow = store();
        shadow.store("s", &["call_1".into()], "loop reasoning");
        let mut body = serde_json::json!({
            "messages": [
                {"role": "user", "content": "task"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "Read", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "ok"}
            ]
        });
        shadow.restore_into(body.as_object_mut().unwrap(), "s");
        assert_eq!(body["messages"][1]["reasoning_content"], "loop reasoning");

        // Different session: no restore.
        let mut body = serde_json::json!({
            "messages": [
                {"role": "user", "content": "task"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "Read", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "ok"}
            ]
        });
        shadow.restore_into(body.as_object_mut().unwrap(), "other");
        assert!(body["messages"][1].get("reasoning_content").is_none());
    }

    #[test]
    fn stress_256_sessions_stay_bounded() {
        let shadow = ReasoningShadowStore::new(ShadowLimits::default());
        let blob = "y".repeat(100_000);
        for index in 0..300 {
            shadow.store(&format!("session_{index}"), &["call_1".into()], &blob);
        }
        let total = shadow.inner.lock().unwrap().total_bytes;
        assert!(total <= 64 * 1024 * 1024);
        assert!(shadow.restore("session_0", &["call_1".into()]).is_none());
        assert!(shadow.restore("session_299", &["call_1".into()]).is_some());
    }
}
