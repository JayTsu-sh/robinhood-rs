//! Rate limiter for policy actions.
//!
//! Two orthogonal limits:
//! * `max_per_sec`        — a leaky-bucket counter of actions/second.
//! * `max_bytes_per_sec`  — a leaky-bucket counter of bytes/second summed
//!   from `EntryRow::size`.
//!
//! Both are approximate: we bill bytes at action-start time rather than
//! measuring real I/O, so a single large copy can temporarily exceed the
//! instantaneous ceiling. Over a multi-second horizon the averages hold.

use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

use crate::model::RateLimit;

/// Shared handle — cheap to clone, all state lives behind an `Arc<Mutex>`.
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<Bucket>>,
}

struct Bucket {
    /// Remaining action budget for the current window.
    tokens: f64,
    /// Remaining byte budget for the current window.
    bytes: f64,
    /// Refill rate per second.
    max_per_sec: Option<f64>,
    max_bytes_per_sec: Option<f64>,
    /// Last time we refilled the buckets.
    last_refill: Instant,
}

impl RateLimiter {
    /// Build a limiter from a [`RateLimit`] spec. Returns `None` when both
    /// limits are `None` (= unlimited).
    pub fn from_spec(spec: &RateLimit) -> Option<Self> {
        let max_per_sec = spec.max_per_sec.map(|v| v as f64);
        let max_bytes_per_sec = spec.max_bytes_per_sec.map(|v| v as f64);
        if max_per_sec.is_none() && max_bytes_per_sec.is_none() {
            return None;
        }
        let bucket = Bucket {
            tokens: max_per_sec.unwrap_or(f64::INFINITY),
            bytes: max_bytes_per_sec.unwrap_or(f64::INFINITY),
            max_per_sec,
            max_bytes_per_sec,
            last_refill: Instant::now(),
        };
        Some(Self {
            inner: Arc::new(Mutex::new(bucket)),
        })
    }

    /// Block until both the action budget and a byte budget for `bytes`
    /// are available; debit them atomically.
    ///
    /// Entries whose size exceeds `max_bytes_per_sec` aren't split — we
    /// wait proportionally (`bytes / rate`) and let the bucket go
    /// negative. That enforces the average rate over time without
    /// starving an oversized entry forever.
    pub async fn acquire(&self, bytes: u64) {
        // Atomically check + debit or compute the next wait.
        loop {
            let wait = {
                let mut b = self.inner.lock().await;
                b.refill();

                let token_ok = b.max_per_sec.is_none() || b.tokens >= 1.0;
                let byte_ok = b.max_bytes_per_sec.is_none() || b.bytes >= bytes as f64;

                if token_ok && byte_ok {
                    if b.max_per_sec.is_some() {
                        b.tokens -= 1.0;
                    }
                    if b.max_bytes_per_sec.is_some() {
                        b.bytes -= bytes as f64;
                    }
                    return;
                }

                let token_wait = match b.max_per_sec {
                    Some(mps) if b.tokens < 1.0 => (1.0 - b.tokens) / mps,
                    _ => 0.0,
                };
                let byte_wait = match b.max_bytes_per_sec {
                    Some(mbps) if b.bytes < bytes as f64 => (bytes as f64 - b.bytes) / mbps,
                    _ => 0.0,
                };
                let wait_secs = token_wait.max(byte_wait).max(0.001);
                Duration::from_secs_f64(wait_secs.min(60.0))
            };
            tokio::time::sleep(wait).await;

            // After sleeping, if the requested bytes exceed the bucket
            // maximum, force a debit (go negative) so we don't spin.
            let mut b = self.inner.lock().await;
            b.refill();
            if let Some(max) = b.max_bytes_per_sec
                && (bytes as f64) > max
                && b.bytes > 0.0
            {
                if b.max_per_sec.is_some() && b.tokens < 1.0 {
                    continue; // still need a token, retry loop
                }
                if b.max_per_sec.is_some() {
                    b.tokens -= 1.0;
                }
                b.bytes -= bytes as f64; // allowed to go negative
                return;
            }
            // Otherwise: fall back to the loop to re-check invariants.
        }
    }
}

impl Bucket {
    fn refill(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_refill).as_secs_f64();
        if dt <= 0.0 {
            return;
        }
        if let Some(r) = self.max_per_sec {
            self.tokens = (self.tokens + dt * r).min(r);
        }
        if let Some(r) = self.max_bytes_per_sec {
            self.bytes = (self.bytes + dt * r).min(r);
        }
        self.last_refill = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_spec_returns_none() {
        let rl = RateLimiter::from_spec(&RateLimit {
            max_per_sec: None,
            max_bytes_per_sec: None,
        });
        assert!(rl.is_none());
    }

    // Use `tokio::time::Instant` for elapsed measurement so the assertion
    // reflects virtual (paused) time, not wall-clock.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn ten_per_sec_cap_takes_at_least_a_second() {
        let rl = RateLimiter::from_spec(&RateLimit {
            max_per_sec: Some(10),
            max_bytes_per_sec: None,
        })
        .unwrap();
        let start = Instant::now();
        for _ in 0..11 {
            rl.acquire(0).await;
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(80),
            "expected >=80ms virtual wait for 11th token, got {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn byte_budget_throttles_large_action() {
        let rl = RateLimiter::from_spec(&RateLimit {
            max_per_sec: None,
            max_bytes_per_sec: Some(1000),
        })
        .unwrap();
        let start = Instant::now();
        rl.acquire(1500).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(400),
            "expected ~500ms virtual wait (1500B at 1000B/s), got {elapsed:?}"
        );
    }
}
