//! Event batcher — wraps [`Deduplicator`] with size / time flush triggers.
//!
//! The batcher accumulates changelog events from the listener's recv loop.
//! A flush is emitted as an [`EventBatch`] when any of these conditions is met:
//!
//! 1. The pending dedup queue reaches `flush_batch_size`.
//! 2. The age of the oldest record in the queue exceeds `flush_interval`.
//! 3. The total pending count crosses the `pending_soft_cap` (force-flush).
//! 4. The caller explicitly requests a flush (e.g. on shutdown).

use std::time::{Duration, Instant};

use crate::dedup::Deduplicator;
use crate::event::ChangelogEventEnvelope;

/// A batch of events emitted by the batcher and sent to the consumer.
#[derive(Debug)]
pub struct EventBatch {
    pub mdt: String,
    pub events: Vec<ChangelogEventEnvelope>,
    pub min_index: u64,
    pub max_index: u64,
}

/// Batcher configuration — matches the defaults in
/// `robinhood_c_changelog_reader_semantics.md` §10.
#[derive(Debug, Clone)]
pub struct BatcherConfig {
    pub flush_interval: Duration,
    pub flush_batch_size: usize,
    pub pending_soft_cap: usize,
}

impl Default for BatcherConfig {
    fn default() -> Self {
        Self {
            flush_interval: Duration::from_secs(5),
            flush_batch_size: 10_000,
            pending_soft_cap: 50_000,
        }
    }
}

/// Combines per-FID dedup with flush triggers.
#[derive(Debug)]
pub struct EventBatcher {
    mdt: String,
    dedup: Deduplicator,
    cfg: BatcherConfig,
    /// Timestamp when the first event was pushed since the last flush.
    window_start: Option<Instant>,
}

impl EventBatcher {
    pub fn new(mdt: impl Into<String>, cfg: BatcherConfig) -> Self {
        Self {
            mdt: mdt.into(),
            dedup: Deduplicator::new(),
            cfg,
            window_start: None,
        }
    }

    /// Push one event through the dedup layer. Returns `true` if a flush is
    /// needed (caller should call [`flush`](Self::flush) next).
    pub fn push(&mut self, env: ChangelogEventEnvelope) -> bool {
        self.dedup.push(env);
        if self.window_start.is_none() {
            self.window_start = Some(Instant::now());
        }
        self.should_flush()
    }

    /// Check if any flush trigger is active without pushing a new event.
    pub fn should_flush(&self) -> bool {
        let pending = self.dedup.pending_count();
        if pending == 0 {
            return false;
        }
        // Size trigger.
        if pending >= self.cfg.flush_batch_size {
            return true;
        }
        // Soft cap trigger.
        if pending >= self.cfg.pending_soft_cap {
            return true;
        }
        // Time trigger.
        if let Some(start) = self.window_start
            && start.elapsed() >= self.cfg.flush_interval
        {
            return true;
        }
        false
    }

    /// Drain the dedup queue and build an [`EventBatch`].
    ///
    /// Returns `None` if no events are pending.
    pub fn flush(&mut self) -> Option<EventBatch> {
        let events = self.dedup.drain();
        self.window_start = None;
        if events.is_empty() {
            return None;
        }
        let min_index = events.first().map(|e| e.index).unwrap_or(0);
        let max_index = events.last().map(|e| e.index).unwrap_or(0);
        Some(EventBatch {
            mdt: self.mdt.clone(),
            events,
            min_index,
            max_index,
        })
    }

    /// True if there are no pending events at all.
    pub fn is_empty(&self) -> bool {
        self.dedup.pending_count() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use lustre_api::LuFid;

    use crate::event::{ChangelogEvent, ChangelogEventEnvelope};

    fn create_env(idx: u64) -> ChangelogEventEnvelope {
        ChangelogEventEnvelope::new(
            "testfs-MDT0000",
            idx,
            1_775_955_820,
            ChangelogEvent::Create {
                fid: LuFid::new(0x200000401, idx as u32, 0),
                parent: LuFid::new(0x200000401, 0x01, 0),
                name: Bytes::from(format!("f{idx}")),
                jobid: None,
                uid: None,
                gid: None,
            },
        )
    }

    #[test]
    fn flush_on_batch_size() {
        let cfg = BatcherConfig {
            flush_batch_size: 3,
            ..Default::default()
        };
        let mut b = EventBatcher::new("m", cfg);
        assert!(!b.push(create_env(1)));
        assert!(!b.push(create_env(2)));
        assert!(b.push(create_env(3))); // triggers flush
        let batch = b.flush().unwrap();
        assert_eq!(batch.events.len(), 3);
        assert_eq!(batch.min_index, 1);
        assert_eq!(batch.max_index, 3);
        assert!(b.is_empty());
    }

    #[test]
    fn flush_on_soft_cap() {
        let cfg = BatcherConfig {
            flush_batch_size: 100_000,
            pending_soft_cap: 2,
            ..Default::default()
        };
        let mut b = EventBatcher::new("m", cfg);
        assert!(!b.push(create_env(1)));
        assert!(b.push(create_env(2))); // soft cap reached
        let batch = b.flush().unwrap();
        assert_eq!(batch.events.len(), 2);
    }

    #[test]
    fn flush_on_time_trigger() {
        let cfg = BatcherConfig {
            flush_interval: Duration::from_millis(0), // immediate
            flush_batch_size: 100_000,
            pending_soft_cap: 100_000,
        };
        let mut b = EventBatcher::new("m", cfg);
        b.push(create_env(1));
        // Even with one event, time trigger fires immediately.
        assert!(b.should_flush());
    }

    #[test]
    fn flush_empty_returns_none() {
        let mut b = EventBatcher::new("m", BatcherConfig::default());
        assert!(b.flush().is_none());
    }

    #[test]
    fn explicit_flush_drains_partial() {
        let mut b = EventBatcher::new("m", BatcherConfig::default());
        b.push(create_env(1));
        b.push(create_env(2));
        // Not yet at batch_size, but explicit flush works.
        let batch = b.flush().unwrap();
        assert_eq!(batch.events.len(), 2);
        assert!(b.is_empty());
    }
}
