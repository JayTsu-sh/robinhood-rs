//! Active HSM state reconciliation.
//!
//! The changelog listener covers most HSM transitions (archive, release,
//! remove) in near real-time. But a few paths drop events:
//!
//! * Lustre may drop records if the changelog ring fills while the
//!   daemon is down.
//! * `lfs hsm_restore` on a file is recorded on the MDT but the in-place
//!   state flip (RELEASED → ARCHIVED) arrives as a structural event
//!   we don't always see cleanly.
//! * Operators use `lfs hsm_set` / `hsm_clear` out-of-band.
//!
//! To reconverge, this poller periodically walks the catalog (files
//! only), calls `llapi_hsm_state_get` for each, and patches
//! `sm_status.hsm_state` when the observed state differs from the
//! catalog's. It is deliberately rate-limited — each state-get is an
//! MDS metadata op, so we page the catalog in small batches with a
//! sleep between them.
//!
//! Controlled by:
//! * `RBH_HSM_POLL_SECS`   — cycle interval in seconds. `0` disables.
//! * `RBH_HSM_POLL_BATCH`  — entries per DB page (default 200).
//! * `RBH_HSM_POLL_PAUSE_MS` — ms to sleep between batches (default 500).

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use lustre_api::LustreApi;
use lustre_api::hsm::HsmState;
use rbh_entry_store::model::{EntryKind, FileSystemId, ObjectId, ScopedEntryRow};
use rbh_entry_store::store::{EntryStore, QueryParam};

pub struct HsmPoller {
    pub entry_store: EntryStore,
    pub lustre: LustreApi,
    pub filesystem_id: FileSystemId,
    pub mount_path: std::path::PathBuf,
    pub tick: Duration,
    pub batch: u64,
    pub pause_between_batches: Duration,
    pub cancel: CancellationToken,
}

impl HsmPoller {
    pub async fn run(self) {
        tracing::info!(
            filesystem = %self.filesystem_id,
            tick_secs = self.tick.as_secs(),
            batch = self.batch,
            pause_ms = self.pause_between_batches.as_millis() as u64,
            "hsm poller started"
        );

        loop {
            if self.cancel.is_cancelled() {
                tracing::info!("hsm poller cancelled");
                return;
            }
            match self.one_cycle().await {
                Ok(stats) => {
                    tracing::info!(
                        filesystem = %self.filesystem_id,
                        scanned = stats.scanned,
                        reconciled = stats.reconciled,
                        errors = stats.errors,
                        "hsm poller cycle complete"
                    );
                    rbh_observability::metrics::HSM_POLL_RECONCILED
                        .with_label_values(&[self.filesystem_id.as_str(), "lustre"])
                        .inc_by(stats.reconciled);
                    rbh_observability::metrics::HSM_POLL_SCANNED
                        .with_label_values(&[self.filesystem_id.as_str(), "lustre"])
                        .inc_by(stats.scanned);
                }
                Err(e) => tracing::warn!(filesystem = %self.filesystem_id, error = %e, "hsm poller cycle error"),
            }

            tokio::select! {
                _ = tokio::time::sleep(self.tick) => {}
                _ = self.cancel.cancelled() => {
                    tracing::info!("hsm poller cancelled mid-sleep");
                    return;
                }
            }
        }
    }

    async fn one_cycle(&self) -> anyhow::Result<CycleStats> {
        let mut stats = CycleStats::default();
        let mut offset: u64 = 0;

        loop {
            if self.cancel.is_cancelled() {
                break;
            }
            // kind = 0 → files only. Other kinds don't have HSM state.
            let scoped_rows = self
                .entry_store
                .query_scoped_page(
                    &self.filesystem_id,
                    "kind = ?",
                    &[QueryParam::Int(EntryKind::File as i64)],
                    Some("fid"),
                    self.batch,
                    offset,
                )
                .await?;
            if scoped_rows.is_empty() {
                break;
            }
            let row_count = scoped_rows.len() as u64;
            offset += row_count;

            for row in scoped_rows {
                stats.scanned += 1;
                match self.reconcile_entry(row).await {
                    Ok(true) => stats.reconciled += 1,
                    Ok(false) => {}
                    Err(e) => {
                        stats.errors += 1;
                        tracing::debug!(error = %e, "hsm state-get failed for entry");
                    }
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(self.pause_between_batches) => {}
                _ = self.cancel.cancelled() => break,
            }

            // Short pages (< batch) mean end-of-table.
            if row_count < self.batch {
                break;
            }
        }

        Ok(stats)
    }

    async fn reconcile_entry(&self, entry: ScopedEntryRow) -> anyhow::Result<bool> {
        let lustre = self.lustre;
        let mount = self.mount_path.clone();
        let ObjectId::Lustre(fid) = *entry.key.object() else {
            anyhow::bail!("HSM poller received a non-Lustre object");
        };

        // Build .lustre/fid/<FID> — valid for stat/ioctl calls, which is
        // what llapi_hsm_state_get uses internally.
        let fid_str = format!("{fid}");
        let stripped = fid_str.trim_start_matches('[').trim_end_matches(']');
        let path = mount.join(".lustre").join("fid").join(stripped);

        let observed = tokio::task::spawn_blocking(move || lustre.hsm_state_get(&path)).await??;

        let observed_label = label_of_state(observed.states);
        let stored_label = entry
            .sm_status
            .get("hsm_state")
            .and_then(|v| v.as_str())
            .unwrap_or("none");

        if observed_label == stored_label {
            return Ok(false);
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let patch = serde_json::json!({
            "hsm_state": observed_label,
            "hsm_last_op": "poll_reconcile",
            "hsm_last_event_ts": now,
            "hsm_dirty": observed.states.contains(HsmState::DIRTY),
        });
        self.entry_store.patch_scoped_sm_status(&entry.key, &patch).await?;
        tracing::info!(
            filesystem = %self.filesystem_id,
            fid = %fid,
            from = stored_label,
            to = observed_label,
            "hsm state reconciled from live state"
        );
        Ok(true)
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct CycleStats {
    scanned: u64,
    reconciled: u64,
    errors: u64,
}

/// Derive the catalog's `hsm_state` label from a live `HsmState` bitmask.
/// Mirrors the mapping used by `build_hsm_patch` in `changelog.rs`.
pub fn label_of_state(s: HsmState) -> &'static str {
    if s.contains(HsmState::RELEASED) {
        "released"
    } else if s.contains(HsmState::ARCHIVED) {
        "archived"
    } else if s.contains(HsmState::EXISTS) {
        "exists"
    } else {
        "none"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_matches_released_first() {
        // RELEASED + ARCHIVED → "released" (released implies archived in Lustre).
        let s = HsmState::ARCHIVED | HsmState::RELEASED | HsmState::EXISTS;
        assert_eq!(label_of_state(s), "released");
    }

    #[test]
    fn label_matches_archived() {
        let s = HsmState::ARCHIVED | HsmState::EXISTS;
        assert_eq!(label_of_state(s), "archived");
    }

    #[test]
    fn label_matches_exists_only() {
        assert_eq!(label_of_state(HsmState::EXISTS), "exists");
    }

    #[test]
    fn label_matches_none() {
        assert_eq!(label_of_state(HsmState::empty()), "none");
    }
}
