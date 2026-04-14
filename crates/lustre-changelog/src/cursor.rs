//! `CursorStore` trait — durable persistence of "last committed changelog
//! record index" per MDT.
//!
//! Phase 1b defines the trait and a [`MemoryCursorStore`] for unit and
//! integration tests. Phase 2's `rbh-entry-store` crate will ship a
//! MariaDB-backed implementation writing to the `rbh_entries.changelog_cursor`
//! table.
//!
//! # Contract
//!
//! * [`CursorStore::get`] returns the highest record index previously committed
//!   for `mdt`, or `None` if nothing has been committed yet. Callers pass
//!   `result.unwrap_or(0)` as the `start_rec` to [`lustre_api::LustreApi::open_changelog`].
//!
//! * [`CursorStore::commit`] persists `rec_id` as the new high-water mark for
//!   `mdt`. Implementations must make the write durable before returning `Ok`:
//!   the listener uses a successful return to decide it can issue
//!   `llapi_changelog_clear` for that index, and a crash after commit but
//!   before the clear is acceptable (the next startup resumes from `rec_id + 1`
//!   and the MDT retains already-cleared records until the next clear).
//!
//! * `commit` must be monotonic: an implementation should silently ignore or
//!   reject a commit whose `rec_id` is lower than the currently stored value.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

/// Error type for cursor storage implementations.
#[derive(Debug, thiserror::Error)]
pub enum CursorError {
    /// Non-monotonic commit: the caller tried to lower the high-water mark.
    #[error("cursor for mdt `{mdt}` cannot go backwards (stored={stored}, attempted={attempted})")]
    NonMonotonic { mdt: String, stored: u64, attempted: u64 },

    /// An I/O or storage-backend failure. Implementations wrap their own error
    /// types behind this with `#[from]` or via `Box<dyn Error>` in the
    /// `message` field when a typed conversion isn't ergonomic.
    #[error("cursor store backend error: {message}")]
    Backend { message: String },
}

/// Durable high-water mark for changelog records per MDT.
///
/// Implementations must be thread-safe and cheap to share via `Arc<dyn CursorStore>`.
#[async_trait]
pub trait CursorStore: Send + Sync + 'static {
    /// Return the last committed record index for `mdt`, or `None` if the
    /// store has never seen this MDT before.
    async fn get(&self, mdt: &str) -> Result<Option<u64>, CursorError>;

    /// Durably record `rec_id` as the new high-water mark for `mdt`.
    ///
    /// Must reject attempts to go backwards (see [`CursorError::NonMonotonic`]).
    async fn commit(&self, mdt: &str, rec_id: u64) -> Result<(), CursorError>;
}

/// In-memory [`CursorStore`] implementation for tests.
///
/// NOT durable across process restarts — unit and integration tests in this
/// crate use it. Production code uses the Phase 2 MariaDB impl.
#[derive(Debug, Default)]
pub struct MemoryCursorStore {
    inner: Mutex<HashMap<String, u64>>,
}

impl MemoryCursorStore {
    /// Create an empty store with no cursors set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Synchronous inspection helper for tests — returns a snapshot of the
    /// current MDT → cursor mapping.
    pub fn snapshot(&self) -> HashMap<String, u64> {
        self.inner.lock().expect("MemoryCursorStore mutex poisoned").clone()
    }
}

#[async_trait]
impl CursorStore for MemoryCursorStore {
    async fn get(&self, mdt: &str) -> Result<Option<u64>, CursorError> {
        let map = self.inner.lock().expect("MemoryCursorStore mutex poisoned");
        Ok(map.get(mdt).copied())
    }

    async fn commit(&self, mdt: &str, rec_id: u64) -> Result<(), CursorError> {
        let mut map = self.inner.lock().expect("MemoryCursorStore mutex poisoned");
        if let Some(&stored) = map.get(mdt)
            && rec_id < stored
        {
            return Err(CursorError::NonMonotonic {
                mdt: mdt.to_owned(),
                stored,
                attempted: rec_id,
            });
        }
        map.insert(mdt.to_owned(), rec_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_empty_returns_none() {
        let store = MemoryCursorStore::new();
        assert_eq!(store.get("testfs-MDT0000").await.unwrap(), None);
    }

    #[tokio::test]
    async fn commit_then_get_roundtrip() {
        let store = MemoryCursorStore::new();
        store.commit("testfs-MDT0000", 42).await.unwrap();
        assert_eq!(store.get("testfs-MDT0000").await.unwrap(), Some(42));
    }

    #[tokio::test]
    async fn commit_monotonic_advance() {
        let store = MemoryCursorStore::new();
        store.commit("testfs-MDT0000", 10).await.unwrap();
        store.commit("testfs-MDT0000", 20).await.unwrap();
        store.commit("testfs-MDT0000", 20).await.unwrap(); // equal is OK
        assert_eq!(store.get("testfs-MDT0000").await.unwrap(), Some(20));
    }

    #[tokio::test]
    async fn commit_rejects_backwards() {
        let store = MemoryCursorStore::new();
        store.commit("testfs-MDT0000", 100).await.unwrap();
        let err = store.commit("testfs-MDT0000", 50).await.unwrap_err();
        match err {
            CursorError::NonMonotonic { mdt, stored, attempted } => {
                assert_eq!(mdt, "testfs-MDT0000");
                assert_eq!(stored, 100);
                assert_eq!(attempted, 50);
            }
            other => panic!("expected NonMonotonic, got {other:?}"),
        }
        // Stored value is unchanged after the failed commit.
        assert_eq!(store.get("testfs-MDT0000").await.unwrap(), Some(100));
    }

    #[tokio::test]
    async fn commits_are_per_mdt() {
        let store = MemoryCursorStore::new();
        store.commit("testfs-MDT0000", 10).await.unwrap();
        store.commit("testfs-MDT0001", 20).await.unwrap();
        assert_eq!(store.get("testfs-MDT0000").await.unwrap(), Some(10));
        assert_eq!(store.get("testfs-MDT0001").await.unwrap(), Some(20));
    }

    #[tokio::test]
    async fn snapshot_returns_all_mdts() {
        let store = MemoryCursorStore::new();
        store.commit("a", 1).await.unwrap();
        store.commit("b", 2).await.unwrap();
        let snap = store.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap.get("a"), Some(&1));
        assert_eq!(snap.get("b"), Some(&2));
    }
}
