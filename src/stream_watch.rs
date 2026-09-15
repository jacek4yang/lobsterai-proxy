//! Explicit upstream/semantic stall detection.
//!
//! Timers are O(1) per chunk. Downstream Anthropic pings must never call
//! [`StreamWatch::on_upstream_bytes`] or [`StreamWatch::on_semantic`]:
//! only real upstream bytes and real semantic deltas (reasoning, text,
//! tool arguments, finish reason) count as progress.

use std::time::Duration;

use tokio::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct StreamTimeouts {
    pub first_event: Duration,
    pub first_semantic: Duration,
    pub stream_idle: Duration,
    pub semantic_idle: Duration,
}

impl StreamTimeouts {
    pub fn from_secs(
        first_event: u64,
        first_semantic: u64,
        stream_idle: u64,
        semantic_idle: u64,
    ) -> Self {
        Self {
            first_event: Duration::from_secs(first_event),
            first_semantic: Duration::from_secs(first_semantic),
            stream_idle: Duration::from_secs(stream_idle),
            semantic_idle: Duration::from_secs(semantic_idle),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallKind {
    FirstEvent,
    FirstSemantic,
    StreamIdle,
    SemanticIdle,
}

impl StallKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FirstEvent => "upstream_first_event_timeout",
            Self::FirstSemantic => "upstream_first_semantic_timeout",
            Self::StreamIdle => "upstream_stream_idle_timeout",
            Self::SemanticIdle => "upstream_semantic_idle_timeout",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::FirstEvent => "upstream produced no stream event before the first-event timeout",
            Self::FirstSemantic => {
                "upstream produced no reasoning/text/tool progress before the first-semantic timeout"
            }
            Self::StreamIdle => "upstream stream was idle (no bytes) beyond the configured limit",
            Self::SemanticIdle => {
                "upstream produced no reasoning/text/tool progress beyond the semantic idle limit"
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct StreamWatch {
    timeouts: StreamTimeouts,
    started: Instant,
    last_byte: Instant,
    last_semantic: Instant,
    got_first_event: bool,
    got_first_semantic: bool,
}

impl StreamWatch {
    pub fn new(timeouts: StreamTimeouts, started: Instant) -> Self {
        Self {
            timeouts,
            started,
            last_byte: started,
            last_semantic: started,
            got_first_event: false,
            got_first_semantic: false,
        }
    }

    pub fn on_upstream_bytes(&mut self, now: Instant) {
        self.last_byte = now;
    }

    pub fn on_sse_event(&mut self) {
        self.got_first_event = true;
    }

    pub fn on_semantic(&mut self, now: Instant) {
        self.got_first_semantic = true;
        self.last_semantic = now;
    }

    pub fn check(&self, now: Instant) -> Option<StallKind> {
        if !self.got_first_event
            && !self.timeouts.first_event.is_zero()
            && now.saturating_duration_since(self.started) >= self.timeouts.first_event
        {
            return Some(StallKind::FirstEvent);
        }
        if !self.got_first_semantic
            && !self.timeouts.first_semantic.is_zero()
            && now.saturating_duration_since(self.started) >= self.timeouts.first_semantic
        {
            return Some(StallKind::FirstSemantic);
        }
        if !self.timeouts.stream_idle.is_zero()
            && now.saturating_duration_since(self.last_byte) >= self.timeouts.stream_idle
        {
            return Some(StallKind::StreamIdle);
        }
        if self.got_first_semantic
            && !self.timeouts.semantic_idle.is_zero()
            && now.saturating_duration_since(self.last_semantic) >= self.timeouts.semantic_idle
        {
            return Some(StallKind::SemanticIdle);
        }
        None
    }

    /// Next instant at which a stall may fire. Far-future when all disabled.
    pub fn next_deadline(&self) -> Instant {
        let far = self.started + Duration::from_secs(60 * 60 * 24 * 365);
        let mut deadline = far;
        if !self.got_first_event && !self.timeouts.first_event.is_zero() {
            deadline = deadline.min(self.started + self.timeouts.first_event);
        }
        if !self.got_first_semantic && !self.timeouts.first_semantic.is_zero() {
            deadline = deadline.min(self.started + self.timeouts.first_semantic);
        }
        if !self.timeouts.stream_idle.is_zero() {
            deadline = deadline.min(self.last_byte + self.timeouts.stream_idle);
        }
        if self.got_first_semantic && !self.timeouts.semantic_idle.is_zero() {
            deadline = deadline.min(self.last_semantic + self.timeouts.semantic_idle);
        }
        deadline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_event_timeout_fires_without_events() {
        let start = Instant::now();
        let watch = StreamWatch::new(StreamTimeouts::from_secs(3, 30, 30, 30), start);
        assert_eq!(watch.check(start + Duration::from_secs(2)), None);
        assert_eq!(
            watch.check(start + Duration::from_secs(3)),
            Some(StallKind::FirstEvent)
        );
    }

    #[test]
    fn first_semantic_timeout_despite_nonsemantic_frames() {
        let start = Instant::now();
        let mut watch = StreamWatch::new(StreamTimeouts::from_secs(3, 5, 2, 4), start);
        watch.on_upstream_bytes(start + Duration::from_secs(1));
        watch.on_sse_event();
        watch.on_upstream_bytes(start + Duration::from_secs(4));
        assert_eq!(watch.check(start + Duration::from_secs(4)), None);
        assert_eq!(
            watch.check(start + Duration::from_secs(5)),
            Some(StallKind::FirstSemantic)
        );
    }

    #[test]
    fn semantic_timer_resets_on_deltas_but_not_pings() {
        let start = Instant::now();
        let mut watch = StreamWatch::new(StreamTimeouts::from_secs(3, 5, 30, 4), start);
        watch.on_upstream_bytes(start);
        watch.on_sse_event();
        watch.on_semantic(start);
        watch.on_upstream_bytes(start + Duration::from_secs(1));
        // A local ping after that: no byte/semantic notification. The
        // semantic idle limit (4s since the only semantic at `start`) fires
        // before the stream idle limit (30s since the last byte at +1s).
        assert_eq!(
            watch.check(start + Duration::from_secs(4)),
            Some(StallKind::SemanticIdle)
        );
        watch.on_semantic(start + Duration::from_secs(3));
        watch.on_upstream_bytes(start + Duration::from_secs(3));
        assert_eq!(watch.check(start + Duration::from_secs(6)), None);
    }

    #[test]
    fn stream_idle_after_bytes_stop() {
        let start = Instant::now();
        let mut watch = StreamWatch::new(StreamTimeouts::from_secs(30, 30, 2, 4), start);
        watch.on_upstream_bytes(start);
        watch.on_sse_event();
        watch.on_semantic(start);
        assert_eq!(
            watch.check(start + Duration::from_secs(2)),
            Some(StallKind::StreamIdle)
        );
    }

    #[test]
    fn deadline_tracks_next_timer() {
        let start = Instant::now();
        let watch = StreamWatch::new(StreamTimeouts::from_secs(3, 5, 2, 4), start);
        let deadline = watch.next_deadline();
        assert!(deadline <= start + Duration::from_secs(3));
    }
}
