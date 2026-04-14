//! Per-FID dedup — three rules from robinhood-C (`chglog_reader.c:743-930`).
//!
//! Events are pushed into a per-FID queue. On each push the deduplicator
//! applies three rule categories in order:
//!
//! 1. **`IGNORE_ALWAYS`** — certain event types are dropped unconditionally
//!    (Mark, Open, NoOpen, ATime). This is typically already handled at parse
//!    time (parse_event returns None), so it's a belt-and-suspenders check.
//! 2. **`IGNORE_CANCEL`** — Create + Unlink (same FID) cancel each other;
//!    Mkdir + Rmdir cancel each other. The zero-footprint pair is removed.
//! 3. **`IGNORE_MASK`** — redundant data-change signals collapse: if the queue
//!    already holds a Close/Trunc/MTime/CTime for this FID, a new one of the
//!    same type is dropped.
//!
//! Ordering: [`Deduplicator::drain`] returns events sorted by record index
//! (monotonic per MDT) so downstream processing is deterministic.

use std::collections::{HashMap, VecDeque};

use lustre_api::LuFid;

use crate::event::{ChangelogEvent, ChangelogEventEnvelope};

/// Per-FID dedup state.
#[derive(Debug, Default)]
pub struct Deduplicator {
    /// Pending events grouped by target FID.
    queues: HashMap<LuFid, VecDeque<ChangelogEventEnvelope>>,
    /// Total count of pending events across all FIDs (avoids summing lengths).
    pending: usize,
}

impl Deduplicator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Total number of pending events across all FIDs.
    pub fn pending_count(&self) -> usize {
        self.pending
    }

    /// Push one event. Returns `true` if the event was accepted (added to the
    /// per-FID queue); `false` if it was dropped by a dedup rule.
    pub fn push(&mut self, env: ChangelogEventEnvelope) -> bool {
        let fid = env.event.fid();

        // M1 fix: IGNORE_ALWAYS is handled by parse_event returning None for
        // Mark/Open/NoOpen/ATime. No separate check needed here — removed the
        // former no-op `is_always_ignored` method.

        let queue = self.queues.entry(fid).or_default();

        // Rule 2: IGNORE_CANCEL — Create + Unlink or Mkdir + Rmdir cancel.
        // H3 fix: only cancel if no Rename or Hardlink sits between the Create
        // and the Unlink, because an intervening Rename means the FID was moved
        // and the Unlink deletes at the new location (Create should survive).
        // H4 fix: also check Unlink's `last_link` — if false, other names for
        // the FID still exist and the Create should not be cancelled.
        if Self::should_cancel(queue, &env.event) {
            // H4 fix: remove only ONE matching partner (the most recent), not all.
            if let Some(pos) = queue.iter().rposition(|e| Self::is_cancel_partner(e, &env.event)) {
                queue.remove(pos);
                self.pending -= 1;
            }
            // Also don't push the new event — both sides cancel.
            if queue.is_empty() {
                self.queues.remove(&fid);
            }
            return false;
        }

        // Rule 3: IGNORE_MASK — redundant data-change events collapse.
        if Self::is_masked(queue, &env.event) {
            return false;
        }

        queue.push_back(env);
        self.pending += 1;
        true
    }

    /// Drain all per-FID queues, returning events sorted by record index.
    pub fn drain(&mut self) -> Vec<ChangelogEventEnvelope> {
        let mut all: Vec<ChangelogEventEnvelope> = self.queues.drain().flat_map(|(_, q)| q.into_iter()).collect();
        all.sort_by_key(|e| e.index);
        self.pending = 0;
        all
    }

    // ── Rule implementations ────────────────────────────────────────────
    //
    // M1 fix: removed the no-op `is_always_ignored` — IGNORE_ALWAYS types
    // (Mark, Open, NoOpen, ATime) are already filtered by `parse_event`
    // returning `None`. No belt-and-suspenders check needed here.

    /// H3 fix: Check if the new event cancels an existing event in the queue.
    /// Create + Unlink → cancel ONLY if:
    ///   (a) `last_link` is true (no other hardlinks remain for the FID), AND
    ///   (b) no Rename or Hardlink sits after the Create in the queue (because
    ///       a rename means the FID was moved and the Unlink deletes at the
    ///       new location — the Create should survive).
    /// Mkdir + Rmdir → cancel unconditionally (directories can't have hardlinks).
    fn should_cancel(queue: &VecDeque<ChangelogEventEnvelope>, new: &ChangelogEvent) -> bool {
        match new {
            ChangelogEvent::Unlink { last_link, .. } => {
                if !last_link {
                    return false; // other hardlinks still exist — don't cancel
                }
                // Find the most recent Create; verify no Rename/Hardlink after it.
                let create_pos = queue
                    .iter()
                    .rposition(|e| matches!(&e.event, ChangelogEvent::Create { .. }));
                if let Some(pos) = create_pos {
                    let has_intervening = queue.iter().skip(pos + 1).any(|e| {
                        matches!(
                            &e.event,
                            ChangelogEvent::Rename { .. } | ChangelogEvent::Hardlink { .. }
                        )
                    });
                    !has_intervening
                } else {
                    false
                }
            }
            ChangelogEvent::Rmdir { .. } => queue.iter().any(|e| matches!(&e.event, ChangelogEvent::Mkdir { .. })),
            _ => false,
        }
    }

    /// H4 fix: partner match for rposition-based single-removal.
    fn is_cancel_partner(existing: &ChangelogEventEnvelope, new: &ChangelogEvent) -> bool {
        match new {
            ChangelogEvent::Unlink { .. } => matches!(&existing.event, ChangelogEvent::Create { .. }),
            ChangelogEvent::Rmdir { .. } => matches!(&existing.event, ChangelogEvent::Mkdir { .. }),
            _ => false,
        }
    }

    /// Check if the new event is a redundant data-change signal that the queue
    /// already holds. Close, Truncate, MTime, CTime, SetAttr on the same FID
    /// collapse: the second one is dropped.
    fn is_masked(queue: &VecDeque<ChangelogEventEnvelope>, new: &ChangelogEvent) -> bool {
        let dominated = |kind: &str| queue.iter().any(|e| e.event.kind_name() == kind);
        match new {
            ChangelogEvent::Close { .. } => dominated("CLOSE"),
            ChangelogEvent::Truncate { .. } => dominated("TRUNC") || dominated("CLOSE"),
            ChangelogEvent::MTime { .. } => dominated("MTIME") || dominated("CLOSE") || dominated("TRUNC"),
            ChangelogEvent::CTime { .. } => dominated("CTIME") || dominated("SATTR") || dominated("CLOSE"),
            ChangelogEvent::SetAttr { .. } => dominated("SATTR"),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use lustre_api::LuFid;

    fn env(index: u64, event: ChangelogEvent) -> ChangelogEventEnvelope {
        ChangelogEventEnvelope::new("testfs-MDT0000", index, 1_775_955_820, event)
    }

    fn fid() -> LuFid {
        LuFid::new(0x200000401, 0x1a, 0)
    }
    fn parent() -> LuFid {
        LuFid::new(0x200000401, 0x01, 0)
    }

    fn create(idx: u64) -> ChangelogEventEnvelope {
        env(
            idx,
            ChangelogEvent::Create {
                fid: fid(),
                parent: parent(),
                name: Bytes::from_static(b"a.txt"),
                jobid: None,
                uid: None,
                gid: None,
            },
        )
    }

    fn unlink(idx: u64) -> ChangelogEventEnvelope {
        env(
            idx,
            ChangelogEvent::Unlink {
                fid: fid(),
                parent: parent(),
                name: Bytes::from_static(b"a.txt"),
                last_link: true,
                hsm_exists: false,
            },
        )
    }

    fn mkdir(idx: u64) -> ChangelogEventEnvelope {
        env(
            idx,
            ChangelogEvent::Mkdir {
                fid: fid(),
                parent: parent(),
                name: Bytes::from_static(b"d"),
                jobid: None,
                uid: None,
                gid: None,
            },
        )
    }

    fn rmdir(idx: u64) -> ChangelogEventEnvelope {
        env(
            idx,
            ChangelogEvent::Rmdir {
                fid: fid(),
                parent: parent(),
                name: Bytes::from_static(b"d"),
            },
        )
    }

    fn close(idx: u64) -> ChangelogEventEnvelope {
        env(
            idx,
            ChangelogEvent::Close {
                fid: fid(),
                parent: parent(),
                name: Bytes::from_static(b"a.txt"),
            },
        )
    }

    fn mtime(idx: u64) -> ChangelogEventEnvelope {
        env(idx, ChangelogEvent::MTime { fid: fid() })
    }

    #[test]
    fn cancel_create_unlink() {
        let mut d = Deduplicator::new();
        assert!(d.push(create(1)));
        assert_eq!(d.pending_count(), 1);
        // Unlink cancels the preceding Create.
        assert!(!d.push(unlink(2)));
        assert_eq!(d.pending_count(), 0);
        assert!(d.drain().is_empty());
    }

    #[test]
    fn cancel_mkdir_rmdir() {
        let mut d = Deduplicator::new();
        assert!(d.push(mkdir(1)));
        assert!(!d.push(rmdir(2)));
        assert_eq!(d.pending_count(), 0);
    }

    #[test]
    fn mask_duplicate_close() {
        let mut d = Deduplicator::new();
        assert!(d.push(close(1)));
        assert!(!d.push(close(2))); // masked by the first
        assert_eq!(d.pending_count(), 1);
        let out = d.drain();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].index, 1);
    }

    #[test]
    fn mask_mtime_by_close() {
        let mut d = Deduplicator::new();
        assert!(d.push(close(1)));
        assert!(!d.push(mtime(2))); // mtime is masked by close
        assert_eq!(d.pending_count(), 1);
    }

    #[test]
    fn no_cancel_across_different_fids() {
        let mut d = Deduplicator::new();
        let create_a = env(
            1,
            ChangelogEvent::Create {
                fid: LuFid::new(0x200000401, 0x10, 0),
                parent: parent(),
                name: Bytes::from_static(b"a"),
                jobid: None,
                uid: None,
                gid: None,
            },
        );
        let unlink_b = env(
            2,
            ChangelogEvent::Unlink {
                fid: LuFid::new(0x200000401, 0x20, 0), // different FID
                parent: parent(),
                name: Bytes::from_static(b"b"),
                last_link: true,
                hsm_exists: false,
            },
        );
        assert!(d.push(create_a));
        assert!(d.push(unlink_b)); // different FID — no cancel
        assert_eq!(d.pending_count(), 2);
    }

    #[test]
    fn drain_returns_sorted_by_index() {
        let mut d = Deduplicator::new();
        // Push out of order (different FIDs so they don't dedup).
        d.push(env(
            5,
            ChangelogEvent::SetAttr {
                fid: LuFid::new(1, 1, 0),
            },
        ));
        d.push(env(
            2,
            ChangelogEvent::SetAttr {
                fid: LuFid::new(1, 2, 0),
            },
        ));
        d.push(env(
            8,
            ChangelogEvent::SetAttr {
                fid: LuFid::new(1, 3, 0),
            },
        ));
        let out = d.drain();
        let indices: Vec<u64> = out.iter().map(|e| e.index).collect();
        assert_eq!(indices, vec![2, 5, 8]);
    }
}
