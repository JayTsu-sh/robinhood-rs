//! `EntryStore` — the main interface to the `rbh_entries` MariaDB database.

use std::time::{SystemTime, UNIX_EPOCH};
use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
};

use sqlx::mysql::MySqlPoolOptions;
use sqlx::{MySql, Pool, Row};
use tracing::{debug, info};

use lustre_api::LuFid;

use crate::error::{Result, StoreError};
use crate::fid_codec;
use crate::model::{
    BaselineState, EntryKey, EntryKind, EntryRow, FileSystemConfig, FileSystemId, FilesystemBaseline, ObjectId,
    ScopedEntryRow, ScopedNamespaceEdge,
};

/// A bind parameter for `legacy_lustre_query_where`. Avoids circular dependency on `rbh-predicate`.
#[derive(Debug, Clone)]
pub enum QueryParam {
    Int(i64),
    Str(String),
}

/// Sort ordering for [`EntryStore::legacy_lustre_aggregate_by`].
#[derive(Debug, Clone, Copy)]
pub enum AggregateSort {
    Count,
    Size,
}

/// Whitelisted column names available for [`EntryStore::legacy_lustre_aggregate_by`].
/// Never lookup from raw user input — go through this enum.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregateKey {
    Uid,
    Gid,
    Projid,
    Kind,
    PoolName,
    ParentFid,
}

impl AggregateKey {
    pub fn as_column(self) -> &'static str {
        match self {
            Self::Uid => "uid",
            Self::Gid => "gid",
            Self::Projid => "projid",
            Self::Kind => "kind",
            Self::PoolName => "pool_name",
            Self::ParentFid => "parent_fid",
        }
    }
}

/// One row returned by [`EntryStore::legacy_lustre_stripe_distribution`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StripeDistRow {
    pub ost_index: u32,
    pub file_count: u64,
    /// Approximate bytes physically on this OST, computed as
    /// `SUM(file.size / file.stripe_count)`.
    pub approx_bytes: u64,
}

/// Connection to the `rbh_entries` MariaDB database.
#[derive(Clone)]
pub struct EntryStore {
    pool: Pool<MySql>,
}

impl EntryStore {
    /// Connect to MariaDB and run migrations.
    #[tracing::instrument(name = "store.connect", skip_all, fields(url = %url))]
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = MySqlPoolOptions::new().max_connections(10).connect(url).await?;

        // Run embedded migrations.
        sqlx::migrate!("./migrations").run(&pool).await?;
        info!("entry store connected and migrated");

        Ok(Self { pool })
    }

    /// Connect with an existing pool (for testing with a shared pool).
    pub fn with_pool(pool: Pool<MySql>) -> Self {
        Self { pool }
    }

    /// Access the underlying pool (for advanced queries / testing).
    pub fn pool(&self) -> &Pool<MySql> {
        &self.pool
    }

    // ── Filesystem-scoped identity (expand phase) ──────────────────────

    /// Register or update one filesystem and its advertised capabilities.
    #[tracing::instrument(name = "store.register_filesystem", skip(self, config), fields(filesystem = %config.id, backend = ?config.backend))]
    pub async fn register_filesystem(&self, config: &FileSystemConfig) -> Result<()> {
        let capabilities = serde_json::to_string(&config.capabilities)?;

        sqlx::query(
            r"INSERT INTO filesystems (id, backend_kind, mount_path, capabilities)
              VALUES (?, ?, ?, ?)
              ON DUPLICATE KEY UPDATE
                backend_kind = VALUES(backend_kind),
                mount_path = VALUES(mount_path),
                capabilities = VALUES(capabilities)",
        )
        .bind(config.id.as_str())
        .bind(config.backend.as_str())
        .bind(config.mount_path.as_os_str().as_bytes())
        .bind(capabilities)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load a registered filesystem by stable identifier.
    #[tracing::instrument(name = "store.get_filesystem", skip(self), fields(filesystem = %id))]
    pub async fn get_filesystem(&self, id: &FileSystemId) -> Result<Option<FileSystemConfig>> {
        let row = sqlx::query("SELECT id, backend_kind, mount_path, capabilities FROM filesystems WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await?;

        row.map(|row| row_to_filesystem(&row)).transpose()
    }

    /// Insert or update a complete filesystem-scoped catalog entry.
    #[tracing::instrument(name = "store.upsert_scoped_entry", skip(self, entry), fields(filesystem = %entry.key.filesystem(), backend = ?entry.key.object().backend()))]
    pub async fn upsert_scoped_entry(&self, entry: &ScopedEntryRow) -> Result<()> {
        if let Some(config) = self.get_filesystem(entry.key.filesystem()).await?
            && config.backend != entry.key.object().backend()
        {
            return Err(StoreError::BackendMismatch {
                filesystem: entry.key.filesystem().clone(),
                configured: config.backend,
                object: entry.key.object().backend(),
            });
        }

        let mut tx = self.pool.begin().await?;
        upsert_scoped_entry_tx(&mut tx, entry).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Upsert a scanner batch into the filesystem-scoped catalog atomically.
    #[tracing::instrument(name = "store.upsert_scoped_batch", skip(self, entries), fields(count = entries.len()))]
    pub async fn upsert_scoped_batch(&self, entries: &[ScopedEntryRow]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let filesystem = entries[0].key.filesystem();
        let config = self
            .get_filesystem(filesystem)
            .await?
            .ok_or_else(|| StoreError::UnknownFilesystem(filesystem.clone()))?;
        let mut tx = self.pool.begin().await?;
        for entry in entries {
            if entry.key.filesystem() != filesystem {
                return Err(StoreError::InvalidObjectIdentity("scoped batch mixes filesystems"));
            }
            if config.backend != entry.key.object().backend() {
                return Err(StoreError::BackendMismatch {
                    filesystem: filesystem.clone(),
                    configured: config.backend,
                    object: entry.key.object().backend(),
                });
            }
            upsert_scoped_entry_tx(&mut tx, entry).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Load a complete catalog entry by filesystem-scoped identity.
    #[tracing::instrument(name = "store.get_scoped_entry", skip(self), fields(filesystem = %key.filesystem(), backend = ?key.object().backend()))]
    pub async fn get_scoped_entry(&self, key: &EntryKey) -> Result<Option<ScopedEntryRow>> {
        let (kind, bytes) = encode_object_id(*key.object());
        let row = sqlx::query(
            r"SELECT entry_data
              FROM scoped_entries
              WHERE filesystem_id = ? AND object_kind = ? AND object_id = ?",
        )
        .bind(key.filesystem().as_str())
        .bind(kind)
        .bind(bytes.as_slice())
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        let entry_data: Vec<u8> = row.try_get("entry_data")?;
        let entry: ScopedEntryRow = serde_json::from_slice(&entry_data)?;
        if entry.key != *key {
            return Err(StoreError::InvalidObjectIdentity(
                "scoped entry key does not match its primary key",
            ));
        }
        Ok(Some(entry))
    }

    /// Delete one filesystem-scoped catalog entry. Repeating the deletion is
    /// intentionally harmless for replayed changelog records.
    pub async fn remove_scoped_entry(&self, key: &EntryKey) -> Result<()> {
        let (kind, bytes) = encode_object_id(*key.object());
        sqlx::query("DELETE FROM scoped_entries WHERE filesystem_id = ? AND object_kind = ? AND object_id = ?")
            .bind(key.filesystem().as_str())
            .bind(kind)
            .bind(bytes.as_slice())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Insert a namespace edge idempotently. This is independent of the
    /// inode-keyed object row so all hard-link names are retained.
    #[tracing::instrument(name = "store.upsert_scoped_namespace_edge", skip(self, edge), fields(filesystem = %edge.filesystem))]
    pub async fn upsert_scoped_namespace_edge(&self, edge: &ScopedNamespaceEdge) -> Result<()> {
        let (parent_kind, parent_id) = encode_object_id(edge.parent);
        let (object_kind, object_id) = encode_object_id(edge.object);
        sqlx::query(
            r"INSERT INTO scoped_namespace_edges
                (filesystem_id, parent_kind, parent_id, name, object_kind, object_id)
              VALUES (?, ?, ?, ?, ?, ?)
              ON DUPLICATE KEY UPDATE object_kind = VALUES(object_kind), object_id = VALUES(object_id)",
        )
        .bind(edge.filesystem.as_str())
        .bind(parent_kind)
        .bind(parent_id.as_slice())
        .bind(edge.name.as_ref())
        .bind(object_kind)
        .bind(object_id.as_slice())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Remove one path edge. Replays are harmless.
    #[tracing::instrument(name = "store.remove_scoped_namespace_edge", skip(self, name), fields(filesystem = %filesystem))]
    pub async fn remove_scoped_namespace_edge(
        &self, filesystem: &FileSystemId, parent: ObjectId, name: &[u8],
    ) -> Result<bool> {
        let (parent_kind, parent_id) = encode_object_id(parent);
        let result = sqlx::query(
            "DELETE FROM scoped_namespace_edges WHERE filesystem_id = ? AND parent_kind = ? AND parent_id = ? AND name = ?",
        )
        .bind(filesystem.as_str())
        .bind(parent_kind)
        .bind(parent_id.as_slice())
        .bind(name)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Atomically add one hard-link edge and increment the inode link count.
    /// An exact replay leaves both records unchanged.
    #[tracing::instrument(name = "store.apply_scoped_hardlink", skip(self, edge), fields(filesystem = %edge.filesystem))]
    pub async fn apply_scoped_hardlink(&self, edge: &ScopedNamespaceEdge, observed_at: i64) -> Result<()> {
        let (parent_kind, parent_id) = encode_object_id(edge.parent);
        let (object_kind, object_id) = encode_object_id(edge.object);
        let mut tx = self.pool.begin().await?;
        let existing = sqlx::query(
            r"SELECT object_kind, object_id FROM scoped_namespace_edges
              WHERE filesystem_id = ? AND parent_kind = ? AND parent_id = ? AND name = ? FOR UPDATE",
        )
        .bind(edge.filesystem.as_str())
        .bind(parent_kind)
        .bind(parent_id.as_slice())
        .bind(edge.name.as_ref())
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = existing {
            let same_object = row.try_get::<u8, _>("object_kind")? == object_kind
                && row.try_get::<Vec<u8>, _>("object_id")?.as_slice() == object_id;
            if same_object {
                tx.commit().await?;
                return Ok(());
            }
            return Err(StoreError::InvalidObjectIdentity(
                "namespace edge belongs to another object",
            ));
        }
        let row = sqlx::query(
            "SELECT entry_data FROM scoped_entries WHERE filesystem_id = ? AND object_kind = ? AND object_id = ? FOR UPDATE",
        )
        .bind(edge.filesystem.as_str())
        .bind(object_kind)
        .bind(object_id.as_slice())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::InvalidObjectIdentity("hard link object is not cataloged"))?;
        let mut entry: ScopedEntryRow = serde_json::from_slice(&row.try_get::<Vec<u8>, _>("entry_data")?)?;
        entry.nlink = entry.nlink.saturating_add(1);
        entry.last_seen = observed_at;
        sqlx::query(
            r"INSERT INTO scoped_namespace_edges
                (filesystem_id, parent_kind, parent_id, name, object_kind, object_id)
              VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(edge.filesystem.as_str())
        .bind(parent_kind)
        .bind(parent_id.as_slice())
        .bind(edge.name.as_ref())
        .bind(object_kind)
        .bind(object_id.as_slice())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE scoped_entries SET entry_data = ?, nlink = ?, last_seen = ? \
             WHERE filesystem_id = ? AND object_kind = ? AND object_id = ?",
        )
        .bind(serde_json::to_string(&entry)?)
        .bind(entry.nlink)
        .bind(entry.last_seen)
        .bind(edge.filesystem.as_str())
        .bind(object_kind)
        .bind(object_id.as_slice())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Atomically remove one namespace edge and update or delete its inode.
    /// An exact replay is a no-op.
    #[tracing::instrument(name = "store.apply_scoped_unlink", skip(self, name), fields(filesystem = %key.filesystem()))]
    pub async fn apply_scoped_unlink(
        &self, key: &EntryKey, parent: ObjectId, name: &[u8], observed_at: i64, directory: bool,
    ) -> Result<()> {
        let (parent_kind, parent_id) = encode_object_id(parent);
        let (object_kind, object_id) = encode_object_id(*key.object());
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query(
            "DELETE FROM scoped_namespace_edges WHERE filesystem_id = ? AND parent_kind = ? AND parent_id = ? AND name = ? AND object_kind = ? AND object_id = ?",
        )
        .bind(key.filesystem().as_str())
        .bind(parent_kind)
        .bind(parent_id.as_slice())
        .bind(name)
        .bind(object_kind)
        .bind(object_id.as_slice())
        .execute(&mut *tx)
        .await?;
        if deleted.rows_affected() == 0 {
            tx.commit().await?;
            return Ok(());
        }
        let row = sqlx::query(
            "SELECT entry_data FROM scoped_entries WHERE filesystem_id = ? AND object_kind = ? AND object_id = ? FOR UPDATE",
        )
        .bind(key.filesystem().as_str())
        .bind(object_kind)
        .bind(object_id.as_slice())
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = row {
            let mut entry: ScopedEntryRow = serde_json::from_slice(&row.try_get::<Vec<u8>, _>("entry_data")?)?;
            if !directory && entry.nlink > 1 {
                entry.nlink -= 1;
                entry.last_seen = observed_at;
                sqlx::query(
                    "UPDATE scoped_entries SET entry_data = ?, nlink = ?, last_seen = ? \
                     WHERE filesystem_id = ? AND object_kind = ? AND object_id = ?",
                )
                .bind(serde_json::to_string(&entry)?)
                .bind(entry.nlink)
                .bind(entry.last_seen)
                .bind(key.filesystem().as_str())
                .bind(object_kind)
                .bind(object_id.as_slice())
                .execute(&mut *tx)
                .await?;
            } else {
                sqlx::query(
                    r"INSERT INTO scoped_removed_entries
                        (filesystem_id, object_kind, object_id, entry_data, rm_time)
                      VALUES (?, ?, ?, ?, ?)
                      ON DUPLICATE KEY UPDATE entry_data = VALUES(entry_data), rm_time = VALUES(rm_time)",
                )
                .bind(key.filesystem().as_str())
                .bind(object_kind)
                .bind(object_id.as_slice())
                .bind(serde_json::to_string(&entry)?)
                .bind(observed_at)
                .execute(&mut *tx)
                .await?;
                sqlx::query("DELETE FROM scoped_entries WHERE filesystem_id = ? AND object_kind = ? AND object_id = ?")
                    .bind(key.filesystem().as_str())
                    .bind(object_kind)
                    .bind(object_id.as_slice())
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Clear only the new filesystem-scoped catalog for one filesystem before
    /// installing a fresh baseline. Legacy Lustre tables are untouched.
    #[tracing::instrument(name = "store.clear_scoped_catalog", skip(self), fields(filesystem = %filesystem))]
    pub async fn clear_scoped_catalog(&self, filesystem: &FileSystemId) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM scoped_stripe_items WHERE filesystem_id = ?")
            .bind(filesystem.as_str())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM scoped_namespace_edges WHERE filesystem_id = ?")
            .bind(filesystem.as_str())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM scoped_entries WHERE filesystem_id = ?")
            .bind(filesystem.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    #[tracing::instrument(name = "store.set_baseline_state", skip(self, reason), fields(filesystem = %filesystem, state = ?state))]
    pub async fn set_baseline_state(
        &self, filesystem: &FileSystemId, state: BaselineState, last_version: Option<u64>, reason: Option<&str>,
    ) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let scan_started = (state == BaselineState::Scanning).then_some(now);
        let completed = (state == BaselineState::Ready).then_some(now);
        sqlx::query(
            r"INSERT INTO filesystem_baselines
                (filesystem_id, state, scan_started_at, completed_at, last_version, invalid_reason)
              VALUES (?, ?, ?, ?, ?, ?)
              ON DUPLICATE KEY UPDATE state = VALUES(state),
                scan_started_at = COALESCE(VALUES(scan_started_at), scan_started_at),
                completed_at = VALUES(completed_at), last_version = VALUES(last_version),
                invalid_reason = VALUES(invalid_reason)",
        )
        .bind(filesystem.as_str())
        .bind(state.as_str())
        .bind(scan_started)
        .bind(completed)
        .bind(last_version)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    #[tracing::instrument(name = "store.get_baseline", skip(self), fields(filesystem = %filesystem))]
    pub async fn get_baseline(&self, filesystem: &FileSystemId) -> Result<Option<FilesystemBaseline>> {
        let row = sqlx::query(
            "SELECT state, scan_started_at, completed_at, last_version, invalid_reason FROM filesystem_baselines WHERE filesystem_id = ?",
        )
        .bind(filesystem.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            let state: String = row.try_get("state")?;
            let state = match state.as_str() {
                "scanning" => BaselineState::Scanning,
                "catching_up" => BaselineState::CatchingUp,
                "ready" => BaselineState::Ready,
                "invalid" => BaselineState::Invalid,
                _ => return Err(StoreError::InvalidObjectIdentity("unknown baseline state")),
            };
            Ok(FilesystemBaseline {
                filesystem: filesystem.clone(),
                state,
                scan_started_at: row.try_get("scan_started_at")?,
                completed_at: row.try_get("completed_at")?,
                last_version: row.try_get("last_version")?,
                invalid_reason: row.try_get("invalid_reason")?,
            })
        })
        .transpose()
    }

    /// Return a deterministic namespace snapshot suitable for independent
    /// comparison with a mounted POSIX walk.
    #[tracing::instrument(name = "store.list_scoped_namespace_edges", skip(self), fields(filesystem = %filesystem))]
    pub async fn list_scoped_namespace_edges(&self, filesystem: &FileSystemId) -> Result<Vec<ScopedNamespaceEdge>> {
        let rows = sqlx::query(
            r"SELECT parent_kind, parent_id, name, object_kind, object_id
              FROM scoped_namespace_edges WHERE filesystem_id = ?
              ORDER BY parent_kind, parent_id, name",
        )
        .bind(filesystem.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ScopedNamespaceEdge {
                    filesystem: filesystem.clone(),
                    parent: decode_object_id(row.try_get("parent_kind")?, &row.try_get::<Vec<u8>, _>("parent_id")?)?,
                    name: bytes::Bytes::from(row.try_get::<Vec<u8>, _>("name")?),
                    object: decode_object_id(row.try_get("object_kind")?, &row.try_get::<Vec<u8>, _>("object_id")?)?,
                })
            })
            .collect()
    }

    /// Count catalog objects in exactly one filesystem.
    #[tracing::instrument(name = "store.scoped_entry_count", skip(self), fields(filesystem = %filesystem))]
    pub async fn scoped_entry_count(&self, filesystem: &FileSystemId) -> Result<u64> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM scoped_entries WHERE filesystem_id = ?")
            .bind(filesystem.as_str())
            .fetch_one(&self.pool)
            .await?;
        Ok(count.max(0) as u64)
    }

    /// Query one filesystem's catalog projection with validated predicate and
    /// ordering fragments produced by `rbh-predicate`.
    #[tracing::instrument(name = "store.query_scoped_page", skip(self, params), fields(filesystem = %filesystem, sql = %where_clause))]
    pub async fn query_scoped_page(
        &self, filesystem: &FileSystemId, where_clause: &str, params: &[QueryParam], order_by: Option<&str>,
        limit: u64, offset: u64,
    ) -> Result<Vec<ScopedEntryRow>> {
        let order_clause = match order_by {
            Some(order) if !order.is_empty() => format!(" ORDER BY {order}"),
            _ => String::new(),
        };
        let sql = format!(
            "SELECT entry_data FROM scoped_entries AS entries \
             WHERE filesystem_id = ? AND ({where_clause}){order_clause} LIMIT ? OFFSET ?"
        );
        let mut query = sqlx::query(&sql).bind(filesystem.as_str());
        for param in params {
            query = match param {
                QueryParam::Int(value) => query.bind(*value),
                QueryParam::Str(value) => query.bind(value.as_str()),
            };
        }
        let rows = query
            .bind(limit as i64)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| serde_json::from_slice(&row.try_get::<Vec<u8>, _>("entry_data")?).map_err(StoreError::from))
            .collect()
    }

    /// Page removed objects belonging to exactly one filesystem.
    #[tracing::instrument(name = "store.list_scoped_removed", skip(self), fields(filesystem = %filesystem))]
    pub async fn list_scoped_removed(
        &self, filesystem: &FileSystemId, since: Option<i64>, limit: u64, offset: u64,
    ) -> Result<Vec<crate::model::ScopedRemovedEntry>> {
        let (sql, has_since) = if since.is_some() {
            (
                "SELECT entry_data, rm_time FROM scoped_removed_entries WHERE filesystem_id = ? AND rm_time >= ? ORDER BY rm_time DESC LIMIT ? OFFSET ?",
                true,
            )
        } else {
            (
                "SELECT entry_data, rm_time FROM scoped_removed_entries WHERE filesystem_id = ? ORDER BY rm_time DESC LIMIT ? OFFSET ?",
                false,
            )
        };
        let mut query = sqlx::query(sql).bind(filesystem.as_str());
        if has_since {
            query = query.bind(since.expect("checked above"));
        }
        let rows = query
            .bind(limit as i64)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(crate::model::ScopedRemovedEntry {
                    entry: serde_json::from_slice(&row.try_get::<Vec<u8>, _>("entry_data")?)?,
                    rm_time: row.try_get("rm_time")?,
                })
            })
            .collect()
    }

    /// Forget one removed object without affecting an identical native id in
    /// another filesystem.
    #[tracing::instrument(name = "store.forget_scoped_removed", skip(self), fields(filesystem = %key.filesystem()))]
    pub async fn forget_scoped_removed(&self, key: &EntryKey) -> Result<bool> {
        let (kind, object_id) = encode_object_id(*key.object());
        let result = sqlx::query(
            "DELETE FROM scoped_removed_entries WHERE filesystem_id = ? AND object_kind = ? AND object_id = ?",
        )
        .bind(key.filesystem().as_str())
        .bind(kind)
        .bind(object_id.as_slice())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Merge status-manager metadata into one scoped object.
    #[tracing::instrument(name = "store.patch_scoped_sm_status", skip(self, patch), fields(filesystem = %key.filesystem()))]
    pub async fn patch_scoped_sm_status(&self, key: &EntryKey, patch: &serde_json::Value) -> Result<bool> {
        let Some(mut entry) = self.get_scoped_entry(key).await? else {
            return Ok(false);
        };
        if let (Some(current), Some(delta)) = (entry.sm_status.as_object_mut(), patch.as_object()) {
            for (name, value) in delta {
                current.insert(name.clone(), value.clone());
            }
        } else {
            entry.sm_status = patch.clone();
        }
        entry.last_seen = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);
        self.upsert_scoped_entry(&entry).await?;
        Ok(true)
    }

    /// Replace the classifier-owned xattr keys on one scoped object.
    #[tracing::instrument(name = "store.update_scoped_xattr", skip(self, tags, clear_keys), fields(filesystem = %key.filesystem()))]
    pub async fn update_scoped_xattr(
        &self, key: &EntryKey, tags: &std::collections::HashMap<String, String>, clear_keys: &[String],
    ) -> Result<bool> {
        let Some(mut entry) = self.get_scoped_entry(key).await? else {
            return Ok(false);
        };
        if entry
            .sm_status
            .get("xattr")
            .and_then(serde_json::Value::as_object)
            .is_none()
        {
            entry.sm_status["xattr"] = serde_json::json!({});
        }
        let xattrs = entry.sm_status["xattr"]
            .as_object_mut()
            .expect("xattr object was installed above");
        for name in clear_keys {
            xattrs.remove(name);
        }
        for (name, value) in tags {
            xattrs.insert(name.clone(), serde_json::Value::String(value.clone()));
        }
        self.upsert_scoped_entry(&entry).await?;
        Ok(true)
    }

    /// Count rows matching a predicate inside one filesystem.
    #[tracing::instrument(name = "store.count_scoped_where", skip(self, params), fields(filesystem = %filesystem, sql = %where_clause))]
    pub async fn count_scoped_where(
        &self, filesystem: &FileSystemId, where_clause: &str, params: &[QueryParam],
    ) -> Result<u64> {
        let sql =
            format!("SELECT COUNT(*) FROM scoped_entries AS entries WHERE filesystem_id = ? AND ({where_clause})");
        let mut query = sqlx::query_scalar::<_, i64>(&sql).bind(filesystem.as_str());
        for param in params {
            query = match param {
                QueryParam::Int(value) => query.bind(*value),
                QueryParam::Str(value) => query.bind(value.as_str()),
            };
        }
        Ok(query.fetch_one(&self.pool).await?.max(0) as u64)
    }

    /// Sum matching object sizes inside one filesystem.
    #[tracing::instrument(name = "store.sum_scoped_size_where", skip(self, params), fields(filesystem = %filesystem, sql = %where_clause))]
    pub async fn sum_scoped_size_where(
        &self, filesystem: &FileSystemId, where_clause: &str, params: &[QueryParam],
    ) -> Result<u64> {
        let sql = format!(
            "SELECT CAST(COALESCE(SUM(size), 0) AS UNSIGNED) FROM scoped_entries AS entries \
             WHERE filesystem_id = ? AND ({where_clause})"
        );
        let mut query = sqlx::query_scalar::<_, u64>(&sql).bind(filesystem.as_str());
        for param in params {
            query = match param {
                QueryParam::Int(value) => query.bind(*value),
                QueryParam::Str(value) => query.bind(value.as_str()),
            };
        }
        Ok(query.fetch_one(&self.pool).await?)
    }

    /// Aggregate a validated catalog column inside one filesystem.
    #[tracing::instrument(name = "store.aggregate_scoped_by", skip(self), fields(filesystem = %filesystem))]
    pub async fn aggregate_scoped_by(
        &self, filesystem: &FileSystemId, key: AggregateKey, order_by: AggregateSort, limit: u64,
    ) -> Result<Vec<(String, u64, u64)>> {
        let order = match order_by {
            AggregateSort::Count => "cnt DESC",
            AggregateSort::Size => "total_size DESC",
        };
        let (group, group_by) = match key {
            AggregateKey::ParentFid => (
                "CONCAT(parent_kind, ':', HEX(parent_id))".to_owned(),
                "parent_kind, parent_id".to_owned(),
            ),
            _ => {
                let column = key.as_column();
                (format!("CAST({column} AS CHAR)"), column.to_owned())
            }
        };
        let sql = format!(
            "SELECT {group} AS grp, COUNT(*) AS cnt, \
             CAST(COALESCE(SUM(size), 0) AS UNSIGNED) AS total_size \
             FROM scoped_entries WHERE filesystem_id = ? GROUP BY {group_by} ORDER BY {order} LIMIT ?"
        );
        let rows = sqlx::query(&sql)
            .bind(filesystem.as_str())
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    row.try_get::<Option<String>, _>("grp")?.unwrap_or_default(),
                    row.try_get::<i64, _>("cnt")? as u64,
                    row.try_get::<u64, _>("total_size").unwrap_or(0),
                ))
            })
            .collect()
    }

    /// Size histogram for files in one filesystem.
    #[tracing::instrument(name = "store.scoped_size_profile", skip(self), fields(filesystem = %filesystem))]
    pub async fn scoped_size_profile(&self, filesystem: &FileSystemId) -> Result<Vec<(String, u64, u64)>> {
        let sql = r"SELECT
              CASE WHEN size = 0 THEN '0' WHEN size < 1024 THEN '<1K'
                   WHEN size < 1024*1024 THEN '1K-1M'
                   WHEN size < 100*1024*1024 THEN '1M-100M'
                   WHEN size < 1024*1024*1024 THEN '100M-1G' ELSE '>=1G' END AS bucket,
              COUNT(*) AS cnt, CAST(COALESCE(SUM(size), 0) AS UNSIGNED) AS total_size,
              CASE WHEN size = 0 THEN 0 WHEN size < 1024 THEN 1
                   WHEN size < 1024*1024 THEN 2 WHEN size < 100*1024*1024 THEN 3
                   WHEN size < 1024*1024*1024 THEN 4 ELSE 5 END AS ord
            FROM scoped_entries WHERE filesystem_id = ? AND kind = 0
            GROUP BY bucket, ord ORDER BY ord";
        let rows = sqlx::query(sql).bind(filesystem.as_str()).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    row.try_get("bucket")?,
                    row.try_get::<i64, _>("cnt")? as u64,
                    row.try_get::<u64, _>("total_size").unwrap_or(0),
                ))
            })
            .collect()
    }

    /// Recursive totals under one backend-native object id.
    #[tracing::instrument(name = "store.scoped_subtree_totals", skip(self), fields(filesystem = %root.filesystem()))]
    pub async fn scoped_subtree_totals(&self, root: &EntryKey) -> Result<(u64, u64)> {
        let (kind, object_id) = encode_object_id(*root.object());
        let sql = r"WITH RECURSIVE descendants (object_kind, object_id) AS (
              SELECT object_kind, object_id FROM scoped_entries
               WHERE filesystem_id = ? AND object_kind = ? AND object_id = ?
              UNION ALL
              SELECT child.object_kind, child.object_id FROM scoped_entries child
              JOIN descendants parent ON child.parent_kind = parent.object_kind AND child.parent_id = parent.object_id
               WHERE child.filesystem_id = ?
            ) SELECT COUNT(*) AS n, CAST(COALESCE(SUM(size), 0) AS UNSIGNED) AS bytes
              FROM scoped_entries WHERE filesystem_id = ? AND (object_kind, object_id) IN
              (SELECT object_kind, object_id FROM descendants)";
        let row = sqlx::query(sql)
            .bind(root.filesystem().as_str())
            .bind(kind)
            .bind(object_id.as_slice())
            .bind(root.filesystem().as_str())
            .bind(root.filesystem().as_str())
            .fetch_one(&self.pool)
            .await?;
        Ok((
            row.try_get::<i64, _>("n")? as u64,
            row.try_get::<u64, _>("bytes").unwrap_or(0),
        ))
    }

    /// Per-OST distribution for one filesystem's scoped stripe metadata.
    #[tracing::instrument(name = "store.scoped_stripe_distribution", skip(self), fields(filesystem = %filesystem))]
    pub async fn scoped_stripe_distribution(&self, filesystem: &FileSystemId) -> Result<Vec<StripeDistRow>> {
        let sql = r"SELECT stripe.ost_index, COUNT(*) AS n,
                   CAST(COALESCE(SUM(entry.size / NULLIF(entry.stripe_count, 0)), 0) AS UNSIGNED) AS bytes
                   FROM scoped_stripe_items stripe JOIN scoped_entries entry
                     ON entry.filesystem_id = stripe.filesystem_id
                    AND entry.object_kind = stripe.object_kind AND entry.object_id = stripe.object_id
                   WHERE stripe.filesystem_id = ? GROUP BY stripe.ost_index ORDER BY bytes DESC";
        let rows = sqlx::query(sql).bind(filesystem.as_str()).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| {
                Ok(StripeDistRow {
                    ost_index: row.try_get("ost_index")?,
                    file_count: row.try_get::<i64, _>("n")? as u64,
                    approx_bytes: row.try_get::<u64, _>("bytes").unwrap_or(0),
                })
            })
            .collect()
    }

    // ── Explicit legacy Lustre compatibility CRUD ──────────────────────

    /// Insert or update a single entry via `INSERT ... ON DUPLICATE KEY UPDATE`.
    #[tracing::instrument(name = "store.legacy_lustre_upsert_entry", skip(self, entry), fields(fid = %entry.fid))]
    pub async fn legacy_lustre_upsert_entry(&self, entry: &EntryRow) -> Result<()> {
        let fid_bin = fid_codec::encode(&entry.fid);
        let parent_bin = entry.parent_fid.as_ref().map(fid_codec::encode);
        let sm_json = serde_json::to_string(&entry.sm_status)?;

        sqlx::query(
            r"INSERT INTO entries
                (fid, parent_fid, name, kind, size, blocks, uid, gid, projid, mode, nlink,
                 atime, mtime, ctime, stripe_count, stripe_size, pool_name, sm_status, last_seen, depth)
              VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
              ON DUPLICATE KEY UPDATE
                parent_fid   = VALUES(parent_fid),
                name         = VALUES(name),
                kind         = VALUES(kind),
                size         = VALUES(size),
                blocks       = VALUES(blocks),
                uid          = VALUES(uid),
                gid          = VALUES(gid),
                projid       = VALUES(projid),
                mode         = VALUES(mode),
                nlink        = VALUES(nlink),
                atime        = VALUES(atime),
                mtime        = VALUES(mtime),
                ctime        = VALUES(ctime),
                stripe_count = VALUES(stripe_count),
                stripe_size  = VALUES(stripe_size),
                pool_name    = VALUES(pool_name),
                sm_status    = VALUES(sm_status),
                last_seen    = VALUES(last_seen),
                depth        = VALUES(depth)",
        )
        .bind(fid_bin.as_slice())
        .bind(parent_bin.as_ref().map(|b| b.as_slice()))
        .bind(entry.name.as_ref())
        .bind(entry.kind as u8)
        .bind(entry.size)
        .bind(entry.blocks)
        .bind(entry.uid)
        .bind(entry.gid)
        .bind(entry.projid)
        .bind(entry.mode)
        .bind(entry.nlink)
        .bind(entry.atime)
        .bind(entry.mtime)
        .bind(entry.ctime)
        .bind(entry.stripe_count)
        .bind(entry.stripe_size)
        .bind(&entry.pool_name)
        .bind(&sm_json)
        .bind(entry.last_seen)
        .bind(entry.depth)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Atomically apply a rename, including moving an overwritten destination
    /// to `removed_entries` before updating the source entry.
    #[tracing::instrument(name = "store.legacy_lustre_rename_entry", skip(self, entry), fields(fid = %entry.fid))]
    pub async fn legacy_lustre_rename_entry(&self, entry: &EntryRow, rm_time: i64) -> Result<()> {
        let fid_bin = fid_codec::encode(&entry.fid);
        let parent_bin = entry.parent_fid.as_ref().map(fid_codec::encode);
        let sm_json = serde_json::to_string(&entry.sm_status)?;
        let mut tx = self.pool.begin().await?;

        if let Some(parent) = parent_bin.as_ref() {
            let displaced: Option<Vec<u8>> = sqlx::query_scalar(
                "SELECT fid FROM entries WHERE parent_fid = ? AND name = ? AND fid != ? LIMIT 1 FOR UPDATE",
            )
            .bind(parent.as_slice())
            .bind(entry.name.as_ref())
            .bind(fid_bin.as_slice())
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(displaced) = displaced {
                sqlx::query(
                    "INSERT INTO removed_entries (fid, parent_fid, name, kind, size, uid, gid, sm_status, rm_time)
                     SELECT fid, parent_fid, name, kind, size, uid, gid, sm_status, ? FROM entries WHERE fid = ?
                     ON DUPLICATE KEY UPDATE rm_time = VALUES(rm_time)",
                )
                .bind(rm_time)
                .bind(&displaced)
                .execute(&mut *tx)
                .await?;
                sqlx::query("DELETE FROM names WHERE fid = ?")
                    .bind(&displaced)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM entries WHERE fid = ?")
                    .bind(&displaced)
                    .execute(&mut *tx)
                    .await?;
            }
        }

        sqlx::query(
            r"INSERT INTO entries
                (fid, parent_fid, name, kind, size, blocks, uid, gid, projid, mode, nlink,
                 atime, mtime, ctime, stripe_count, stripe_size, pool_name, sm_status, last_seen, depth)
              VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
              ON DUPLICATE KEY UPDATE
                parent_fid = VALUES(parent_fid), name = VALUES(name), kind = VALUES(kind),
                size = VALUES(size), blocks = VALUES(blocks), uid = VALUES(uid), gid = VALUES(gid),
                projid = VALUES(projid), mode = VALUES(mode), nlink = VALUES(nlink),
                atime = VALUES(atime), mtime = VALUES(mtime), ctime = VALUES(ctime),
                stripe_count = VALUES(stripe_count), stripe_size = VALUES(stripe_size),
                pool_name = VALUES(pool_name), sm_status = VALUES(sm_status),
                last_seen = VALUES(last_seen), depth = VALUES(depth)",
        )
        .bind(fid_bin.as_slice())
        .bind(parent_bin.as_ref().map(|value| value.as_slice()))
        .bind(entry.name.as_ref())
        .bind(entry.kind as u8)
        .bind(entry.size)
        .bind(entry.blocks)
        .bind(entry.uid)
        .bind(entry.gid)
        .bind(entry.projid)
        .bind(entry.mode)
        .bind(entry.nlink)
        .bind(entry.atime)
        .bind(entry.mtime)
        .bind(entry.ctime)
        .bind(entry.stripe_count)
        .bind(entry.stripe_size)
        .bind(&entry.pool_name)
        .bind(&sm_json)
        .bind(entry.last_seen)
        .bind(entry.depth)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Get one entry by FID.
    #[tracing::instrument(name = "store.legacy_lustre_get_entry", skip(self), fields(fid = %fid))]
    pub async fn legacy_lustre_get_entry(&self, fid: &LuFid) -> Result<Option<EntryRow>> {
        let fid_bin = fid_codec::encode(fid);
        let row = sqlx::query(
            "SELECT fid, parent_fid, name, kind, size, blocks, uid, gid, projid, mode, nlink,
                    atime, mtime, ctime, stripe_count, stripe_size, pool_name, sm_status, last_seen, depth
             FROM entries WHERE fid = ?",
        )
        .bind(fid_bin.as_slice())
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(r) => Ok(Some(row_to_entry(&r)?)),
            None => Ok(None),
        }
    }

    /// Delete an entry by FID (moves to `removed_entries` if `rm_time` is set).
    #[tracing::instrument(name = "store.legacy_lustre_remove_entry", skip(self), fields(fid = %fid))]
    pub async fn legacy_lustre_remove_entry(&self, fid: &LuFid, rm_time: i64) -> Result<()> {
        let fid_bin = fid_codec::encode(fid);

        // Move to removed_entries in one transaction.
        let mut tx = self.pool.begin().await?;

        // H3 fix: check rows_affected to distinguish "moved" from "not found".
        let result = sqlx::query(
            "INSERT INTO removed_entries (fid, parent_fid, name, kind, size, uid, gid, sm_status, rm_time)
             SELECT fid, parent_fid, name, kind, size, uid, gid, sm_status, ?
             FROM entries WHERE fid = ?
             ON DUPLICATE KEY UPDATE rm_time = VALUES(rm_time)",
        )
        .bind(rm_time)
        .bind(fid_bin.as_slice())
        .execute(&mut *tx)
        .await?;

        if result.rows_affected() == 0 {
            tracing::warn!(fid = %fid, "legacy_lustre_remove_entry: FID not found in entries table");
        }

        sqlx::query("DELETE FROM entries WHERE fid = ?")
            .bind(fid_bin.as_slice())
            .execute(&mut *tx)
            .await?;

        // Also clean up names table.
        sqlx::query("DELETE FROM names WHERE fid = ?")
            .bind(fid_bin.as_slice())
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        debug!(fid = %fid, "entry moved to removed_entries");
        Ok(())
    }

    /// Upsert a batch of entries in one transaction.
    /// Used by changelog ingest and fs-scan.
    #[tracing::instrument(name = "store.legacy_lustre_upsert_batch", skip(self, entries), fields(count = entries.len()))]
    pub async fn legacy_lustre_upsert_batch(&self, entries: &[EntryRow]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for entry in entries {
            let fid_bin = fid_codec::encode(&entry.fid);
            let parent_bin = entry.parent_fid.as_ref().map(fid_codec::encode);
            let sm_json = serde_json::to_string(&entry.sm_status)?;

            sqlx::query(
                r"INSERT INTO entries
                    (fid, parent_fid, name, kind, size, blocks, uid, gid, projid, mode, nlink,
                     atime, mtime, ctime, stripe_count, stripe_size, pool_name, sm_status, last_seen, depth)
                  VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                  ON DUPLICATE KEY UPDATE
                    parent_fid   = VALUES(parent_fid),
                    name         = VALUES(name),
                    kind         = VALUES(kind),
                    size         = VALUES(size),
                    blocks       = VALUES(blocks),
                    uid          = VALUES(uid),
                    gid          = VALUES(gid),
                    projid       = VALUES(projid),
                    mode         = VALUES(mode),
                    nlink        = VALUES(nlink),
                    atime        = VALUES(atime),
                    mtime        = VALUES(mtime),
                    ctime        = VALUES(ctime),
                    stripe_count = VALUES(stripe_count),
                    stripe_size  = VALUES(stripe_size),
                    pool_name    = VALUES(pool_name),
                    sm_status    = VALUES(sm_status),
                    last_seen    = VALUES(last_seen),
                    depth        = VALUES(depth)",
            )
            .bind(fid_bin.as_slice())
            .bind(parent_bin.as_ref().map(|b| b.as_slice()))
            .bind(entry.name.as_ref())
            .bind(entry.kind as u8)
            .bind(entry.size)
            .bind(entry.blocks)
            .bind(entry.uid)
            .bind(entry.gid)
            .bind(entry.projid)
            .bind(entry.mode)
            .bind(entry.nlink)
            .bind(entry.atime)
            .bind(entry.mtime)
            .bind(entry.ctime)
            .bind(entry.stripe_count)
            .bind(entry.stripe_size)
            .bind(&entry.pool_name)
            .bind(&sm_json)
            .bind(entry.last_seen)
            .bind(entry.depth)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        debug!(count = entries.len(), "batch upserted");
        Ok(())
    }

    /// Atomically persist one Lustre scan batch in both compatibility and
    /// filesystem-scoped catalogs.
    #[tracing::instrument(name = "store.upsert_lustre_scan_batch", skip(self, entries), fields(filesystem = %filesystem, count = entries.len()))]
    pub async fn upsert_lustre_scan_batch(&self, filesystem: &FileSystemId, entries: &[EntryRow]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let config = self
            .get_filesystem(filesystem)
            .await?
            .ok_or_else(|| StoreError::UnknownFilesystem(filesystem.clone()))?;
        if config.backend != crate::model::BackendKind::Lustre {
            return Err(StoreError::BackendMismatch {
                filesystem: filesystem.clone(),
                configured: config.backend,
                object: crate::model::BackendKind::Lustre,
            });
        }
        let mut tx = self.pool.begin().await?;
        for entry in entries {
            let fid_bin = fid_codec::encode(&entry.fid);
            let parent_bin = entry.parent_fid.as_ref().map(fid_codec::encode);
            let sm_json = serde_json::to_string(&entry.sm_status)?;
            sqlx::query(
                r"INSERT INTO entries
                    (fid, parent_fid, name, kind, size, blocks, uid, gid, projid, mode, nlink,
                     atime, mtime, ctime, stripe_count, stripe_size, pool_name, sm_status, last_seen, depth)
                  VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                  ON DUPLICATE KEY UPDATE
                    parent_fid = VALUES(parent_fid), name = VALUES(name), kind = VALUES(kind),
                    size = VALUES(size), blocks = VALUES(blocks), uid = VALUES(uid), gid = VALUES(gid),
                    projid = VALUES(projid), mode = VALUES(mode), nlink = VALUES(nlink),
                    atime = VALUES(atime), mtime = VALUES(mtime), ctime = VALUES(ctime),
                    stripe_count = VALUES(stripe_count), stripe_size = VALUES(stripe_size),
                    pool_name = VALUES(pool_name), sm_status = VALUES(sm_status),
                    last_seen = VALUES(last_seen), depth = VALUES(depth)",
            )
            .bind(fid_bin.as_slice())
            .bind(parent_bin.as_ref().map(|value| value.as_slice()))
            .bind(entry.name.as_ref())
            .bind(entry.kind as u8)
            .bind(entry.size)
            .bind(entry.blocks)
            .bind(entry.uid)
            .bind(entry.gid)
            .bind(entry.projid)
            .bind(entry.mode)
            .bind(entry.nlink)
            .bind(entry.atime)
            .bind(entry.mtime)
            .bind(entry.ctime)
            .bind(entry.stripe_count)
            .bind(entry.stripe_size)
            .bind(&entry.pool_name)
            .bind(&sm_json)
            .bind(entry.last_seen)
            .bind(entry.depth)
            .execute(&mut *tx)
            .await?;

            let scoped = ScopedEntryRow::from_lustre(filesystem.clone(), entry);
            upsert_scoped_entry_tx(&mut tx, &scoped).await?;
            sqlx::query("DELETE FROM stripe_items WHERE fid = ?")
                .bind(fid_bin.as_slice())
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "DELETE FROM scoped_stripe_items WHERE filesystem_id = ? AND object_kind = 0 AND object_id = ?",
            )
            .bind(filesystem.as_str())
            .bind(fid_bin.as_slice())
            .execute(&mut *tx)
            .await?;
            for (stripe_index, ost_index) in entry.stripe_items.iter().copied().enumerate() {
                sqlx::query("INSERT INTO stripe_items (fid, stripe_index, ost_index) VALUES (?, ?, ?)")
                    .bind(fid_bin.as_slice())
                    .bind(stripe_index as u16)
                    .bind(ost_index)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(
                    r"INSERT INTO scoped_stripe_items
                        (filesystem_id, object_kind, object_id, stripe_index, ost_index)
                      VALUES (?, 0, ?, ?, ?)",
                )
                .bind(filesystem.as_str())
                .bind(fid_bin.as_slice())
                .bind(stripe_index as u16)
                .bind(ost_index)
                .execute(&mut *tx)
                .await?;
            }
            if let Some(parent) = entry.parent_fid {
                let parent_id = fid_codec::encode(&parent);
                sqlx::query(
                    r"INSERT INTO scoped_namespace_edges
                        (filesystem_id, parent_kind, parent_id, name, object_kind, object_id)
                      VALUES (?, 0, ?, ?, 0, ?)
                      ON DUPLICATE KEY UPDATE object_kind = VALUES(object_kind), object_id = VALUES(object_id)",
                )
                .bind(filesystem.as_str())
                .bind(parent_id.as_slice())
                .bind(entry.name.as_ref())
                .bind(fid_bin.as_slice())
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Persist one Lustre observation into both the scoped catalog and the
    /// temporary legacy compatibility table.
    #[tracing::instrument(name = "store.upsert_lustre_entry", skip(self, entry), fields(filesystem = %filesystem, fid = %entry.fid))]
    pub async fn upsert_lustre_entry(&self, filesystem: &FileSystemId, entry: &EntryRow) -> Result<()> {
        self.upsert_lustre_scan_batch(filesystem, std::slice::from_ref(entry))
            .await
    }

    /// Load a Lustre object through its filesystem-scoped identity.
    #[tracing::instrument(name = "store.get_lustre_entry", skip(self), fields(filesystem = %filesystem, fid = %fid))]
    pub async fn get_lustre_entry(&self, filesystem: &FileSystemId, fid: &LuFid) -> Result<Option<EntryRow>> {
        let key = EntryKey::new(filesystem.clone(), ObjectId::Lustre(*fid));
        Ok(self
            .get_scoped_entry(&key)
            .await?
            .and_then(|entry| entry.to_lustre_compat()))
    }

    /// Move a scoped Lustre object into the filesystem-scoped removed set.
    /// The legacy table is updated first as a compatibility side effect; a
    /// failed scoped transaction prevents checkpoint advancement and replay
    /// converges both projections.
    #[tracing::instrument(name = "store.remove_lustre_entry", skip(self), fields(filesystem = %filesystem, fid = %fid))]
    pub async fn remove_lustre_entry(&self, filesystem: &FileSystemId, fid: &LuFid, rm_time: i64) -> Result<()> {
        let key = EntryKey::new(filesystem.clone(), ObjectId::Lustre(*fid));
        let scoped = self.get_scoped_entry(&key).await?;
        self.legacy_lustre_remove_entry(fid, rm_time).await?;
        let Some(entry) = scoped else {
            return Ok(());
        };
        let (object_kind, object_id) = encode_object_id(*key.object());
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM scoped_stripe_items WHERE filesystem_id = ? AND object_kind = ? AND object_id = ?")
            .bind(filesystem.as_str())
            .bind(object_kind)
            .bind(object_id.as_slice())
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            r"INSERT INTO scoped_removed_entries (filesystem_id, object_kind, object_id, entry_data, rm_time)
              VALUES (?, ?, ?, ?, ?)
              ON DUPLICATE KEY UPDATE entry_data = VALUES(entry_data), rm_time = VALUES(rm_time)",
        )
        .bind(filesystem.as_str())
        .bind(object_kind)
        .bind(object_id.as_slice())
        .bind(serde_json::to_string(&entry)?)
        .bind(rm_time)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM scoped_namespace_edges WHERE filesystem_id = ? AND object_kind = ? AND object_id = ?")
            .bind(filesystem.as_str())
            .bind(object_kind)
            .bind(object_id.as_slice())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM scoped_entries WHERE filesystem_id = ? AND object_kind = ? AND object_id = ?")
            .bind(filesystem.as_str())
            .bind(object_kind)
            .bind(object_id.as_slice())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Apply a Lustre rename to both projections while keeping namespace
    /// edges inside the selected filesystem.
    #[tracing::instrument(name = "store.rename_lustre_entry", skip(self, entry), fields(filesystem = %filesystem, fid = %entry.fid))]
    pub async fn rename_lustre_entry(&self, filesystem: &FileSystemId, entry: &EntryRow, rm_time: i64) -> Result<()> {
        let displaced = if let Some(parent) = entry.parent_fid {
            let parent_id = fid_codec::encode(&parent);
            let row = sqlx::query(
                r"SELECT object_id FROM scoped_namespace_edges
                  WHERE filesystem_id = ? AND parent_kind = 0 AND parent_id = ? AND name = ?
                    AND NOT (object_kind = 0 AND object_id = ?) LIMIT 1",
            )
            .bind(filesystem.as_str())
            .bind(parent_id.as_slice())
            .bind(entry.name.as_ref())
            .bind(fid_codec::encode(&entry.fid).as_slice())
            .fetch_optional(&self.pool)
            .await?;
            row.and_then(|row| fid_codec::decode(&row.try_get::<Vec<u8>, _>("object_id").ok()?))
        } else {
            None
        };
        self.legacy_lustre_rename_entry(entry, rm_time).await?;
        if let Some(fid) = displaced {
            self.remove_lustre_entry(filesystem, &fid, rm_time).await?;
        }
        let object_id = fid_codec::encode(&entry.fid);
        sqlx::query("DELETE FROM scoped_namespace_edges WHERE filesystem_id = ? AND object_kind = 0 AND object_id = ?")
            .bind(filesystem.as_str())
            .bind(object_id.as_slice())
            .execute(&self.pool)
            .await?;
        self.upsert_scoped_entry(&ScopedEntryRow::from_lustre(filesystem.clone(), entry))
            .await?;
        if let Some(parent) = entry.parent_fid {
            self.upsert_scoped_namespace_edge(&ScopedNamespaceEdge {
                filesystem: filesystem.clone(),
                parent: ObjectId::Lustre(parent),
                name: entry.name.clone(),
                object: ObjectId::Lustre(entry.fid),
            })
            .await?;
        }
        Ok(())
    }

    /// Look up an entry by (parent_fid, name). Returns the FID if found.
    ///
    /// Used by changelog ingest to detect rename-overwrite: when a rename
    /// destination already exists, the displaced entry must be removed.
    #[tracing::instrument(name = "store.legacy_lustre_lookup_by_parent_name", skip(self))]
    pub async fn legacy_lustre_lookup_by_parent_name(&self, parent_fid: &LuFid, name: &[u8]) -> Result<Option<LuFid>> {
        let parent_bin = fid_codec::encode(parent_fid);
        let row = sqlx::query("SELECT fid FROM entries WHERE parent_fid = ? AND name = ? LIMIT 1")
            .bind(parent_bin.as_slice())
            .bind(name)
            .fetch_optional(&self.pool)
            .await?;

        match row {
            Some(r) => {
                let fid_bytes: Vec<u8> = r.try_get("fid")?;
                Ok(fid_codec::decode(&fid_bytes))
            }
            None => Ok(None),
        }
    }

    /// Count entries in the catalog.
    #[tracing::instrument(name = "store.legacy_lustre_entry_count", skip(self))]
    pub async fn legacy_lustre_entry_count(&self) -> Result<u64> {
        let row = sqlx::query("SELECT COUNT(*) as cnt FROM entries")
            .fetch_one(&self.pool)
            .await?;
        let cnt: i64 = row.try_get("cnt")?;
        Ok(cnt.max(0) as u64)
    }

    /// Query entries matching a SQL WHERE clause with positional `?` params.
    ///
    /// Used by PolicyRunTask to push predicate scope down to MariaDB.
    /// Params are `(i64 | String)` bound in order.
    #[tracing::instrument(name = "store.legacy_lustre_query_where", skip(self, params), fields(sql = %where_clause))]
    pub async fn legacy_lustre_query_where(
        &self, where_clause: &str, params: &[QueryParam], limit: u64,
    ) -> Result<Vec<EntryRow>> {
        self.legacy_lustre_query_page(where_clause, params, None, limit, 0)
            .await
    }

    /// Paginated query with optional ORDER BY. `order_by` must be a
    /// pre-validated SQL fragment (column name + ASC/DESC). Callers build
    /// it via [`SortKey::to_sql_fragment`] — never from raw user input.
    #[tracing::instrument(name = "store.legacy_lustre_query_page", skip(self, params), fields(sql = %where_clause))]
    pub async fn legacy_lustre_query_page(
        &self, where_clause: &str, params: &[QueryParam], order_by: Option<&str>, limit: u64, offset: u64,
    ) -> Result<Vec<EntryRow>> {
        let order_clause = match order_by {
            Some(o) if !o.is_empty() => format!(" ORDER BY {o}"),
            _ => String::new(),
        };
        let sql = format!(
            "SELECT fid, parent_fid, name, kind, size, blocks, uid, gid, projid, \
             mode, nlink, atime, mtime, ctime, stripe_count, stripe_size, \
             pool_name, sm_status, last_seen, depth \
             FROM entries WHERE {where_clause}{order_clause} LIMIT ? OFFSET ?"
        );
        let mut query = sqlx::query(&sql);
        for p in params {
            query = match p {
                QueryParam::Int(n) => query.bind(*n),
                QueryParam::Str(s) => query.bind(s.as_str()),
            };
        }
        query = query.bind(limit as i64).bind(offset as i64);
        let rows = query.fetch_all(&self.pool).await?;
        rows.iter().map(row_to_entry).collect()
    }

    /// Group entries by a column, returning `(key, count, total_size)`
    /// tuples. `group_col` MUST be a validated column name (never raw user
    /// input) — callers go through [`AggregateKey::as_column`].
    #[tracing::instrument(name = "store.legacy_lustre_aggregate_by", skip(self))]
    pub async fn legacy_lustre_aggregate_by(
        &self, group_col: &str, order_by: AggregateSort, limit: u64,
    ) -> Result<Vec<(String, u64, u64)>> {
        let order = match order_by {
            AggregateSort::Count => "cnt DESC",
            AggregateSort::Size => "total_size DESC",
        };
        // parent_fid is BINARY(16); all other whitelisted keys are
        // integer or VARCHAR. HEX() renders the FID bytes as a printable
        // string so the JSON payload stays readable.
        let grp_expr = if group_col == "parent_fid" {
            format!("HEX({group_col})")
        } else {
            format!("CAST({group_col} AS CHAR)")
        };
        let sql = format!(
            "SELECT {grp_expr} AS grp, COUNT(*) AS cnt, \
             CAST(COALESCE(SUM(size), 0) AS UNSIGNED) AS total_size \
             FROM entries GROUP BY {group_col} \
             ORDER BY {order} LIMIT ?"
        );
        let rows = sqlx::query(&sql).bind(limit as i64).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|r| {
                let key: Option<String> = r.try_get("grp").ok();
                let cnt: i64 = r.try_get("cnt")?;
                let total: u64 = r.try_get::<u64, _>("total_size").unwrap_or(0);
                Ok((key.unwrap_or_default(), cnt as u64, total))
            })
            .collect()
    }

    /// Size-histogram: bucket entries by log2-ish size ranges
    /// (`0`, `1-1K`, `1K-1M`, `1M-100M`, `100M-1G`, `>=1G`).
    /// Returns `(label, count, total_size)` tuples, bucket order preserved.
    #[tracing::instrument(name = "store.legacy_lustre_size_profile", skip(self))]
    pub async fn legacy_lustre_size_profile(&self) -> Result<Vec<(String, u64, u64)>> {
        let sql = "
            SELECT
              CASE
                WHEN size = 0                              THEN '0'
                WHEN size < 1024                           THEN '<1K'
                WHEN size < 1024*1024                      THEN '1K-1M'
                WHEN size < 100 * 1024 * 1024              THEN '1M-100M'
                WHEN size < 1024 * 1024 * 1024             THEN '100M-1G'
                ELSE                                            '>=1G'
              END AS bucket,
              COUNT(*)                                          AS cnt,
              CAST(COALESCE(SUM(size), 0) AS UNSIGNED)          AS total_size,
              -- sort key to preserve bucket order in the output
              CASE
                WHEN size = 0                              THEN 0
                WHEN size < 1024                           THEN 1
                WHEN size < 1024*1024                      THEN 2
                WHEN size < 100 * 1024 * 1024              THEN 3
                WHEN size < 1024 * 1024 * 1024             THEN 4
                ELSE                                            5
              END AS ord
            FROM entries
            WHERE kind = 0
            GROUP BY bucket, ord
            ORDER BY ord
        ";
        let rows = sqlx::query(sql).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|r| {
                let bucket: String = r.try_get("bucket")?;
                let cnt: i64 = r.try_get("cnt")?;
                let total: u64 = r.try_get::<u64, _>("total_size").unwrap_or(0);
                Ok((bucket, cnt as u64, total))
            })
            .collect()
    }

    /// Merge a JSON patch into the `sm_status` of one entry. Creates
    /// the column as `{}` first if it's NULL. Missing rows are a no-op
    /// (HSM events can arrive before the initial scan); returns whether
    /// a row was touched.
    #[tracing::instrument(name = "store.legacy_lustre_patch_sm_status", skip(self, patch), fields(fid = %fid))]
    pub async fn legacy_lustre_patch_sm_status(&self, fid: &LuFid, patch: &serde_json::Value) -> Result<bool> {
        let bytes = fid_codec::encode(fid);
        // Pull, merge, write. Doing this in-app (not via MySQL
        // JSON_MERGE_PATCH) keeps the logic portable and testable.
        let row = sqlx::query("SELECT sm_status FROM entries WHERE fid = ?")
            .bind(&bytes[..])
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(false);
        };
        let sm_bytes: Option<Vec<u8>> = row.try_get("sm_status")?;
        let mut current: serde_json::Value = match sm_bytes {
            Some(b) if !b.is_empty() => serde_json::from_slice(&b)?,
            _ => serde_json::Value::Object(serde_json::Map::new()),
        };
        if let (Some(obj), Some(patch_obj)) = (current.as_object_mut(), patch.as_object()) {
            for (k, v) in patch_obj {
                obj.insert(k.clone(), v.clone());
            }
        } else {
            current = patch.clone();
        }
        let merged = serde_json::to_string(&current)?;
        sqlx::query("UPDATE entries SET sm_status = ?, last_seen = ? WHERE fid = ?")
            .bind(merged)
            .bind(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
            )
            .bind(&bytes[..])
            .execute(&self.pool)
            .await?;
        Ok(true)
    }

    /// Page the `removed_entries` table ordered by `rm_time` DESC
    /// (newest first). Optional `since` filters rm_time >= since.
    #[tracing::instrument(name = "store.legacy_lustre_list_removed", skip(self))]
    pub async fn legacy_lustre_list_removed(
        &self, since: Option<i64>, limit: u64, offset: u64,
    ) -> Result<Vec<crate::model::RemovedEntry>> {
        let (sql, use_since) = match since {
            Some(_) => (
                "SELECT fid, parent_fid, name, kind, size, uid, gid, sm_status, rm_time \
                 FROM removed_entries WHERE rm_time >= ? \
                 ORDER BY rm_time DESC LIMIT ? OFFSET ?",
                true,
            ),
            None => (
                "SELECT fid, parent_fid, name, kind, size, uid, gid, sm_status, rm_time \
                 FROM removed_entries \
                 ORDER BY rm_time DESC LIMIT ? OFFSET ?",
                false,
            ),
        };
        let mut q = sqlx::query(sql);
        if use_since {
            q = q.bind(since.unwrap());
        }
        q = q.bind(limit as i64).bind(offset as i64);
        let rows = q.fetch_all(&self.pool).await?;
        rows.iter().map(row_to_removed).collect()
    }

    /// Purge a removed-entry row (after operator confirmation or
    /// `rbh undelete` success). Returns whether a row was deleted.
    #[tracing::instrument(name = "store.legacy_lustre_forget_removed", skip(self), fields(fid = %fid))]
    pub async fn legacy_lustre_forget_removed(&self, fid: &LuFid) -> Result<bool> {
        let bytes = fid_codec::encode(fid);
        let res = sqlx::query("DELETE FROM removed_entries WHERE fid = ?")
            .bind(&bytes[..])
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Look up one removed entry by FID.
    #[tracing::instrument(name = "store.legacy_lustre_get_removed", skip(self), fields(fid = %fid))]
    pub async fn legacy_lustre_get_removed(&self, fid: &LuFid) -> Result<Option<crate::model::RemovedEntry>> {
        let bytes = fid_codec::encode(fid);
        let row = sqlx::query(
            "SELECT fid, parent_fid, name, kind, size, uid, gid, sm_status, rm_time \
             FROM removed_entries WHERE fid = ?",
        )
        .bind(&bytes[..])
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(row_to_removed).transpose()
    }

    /// Count matching rows (no limit/offset). Used for paginated responses
    /// that want a total count.
    #[tracing::instrument(name = "store.legacy_lustre_count_where", skip(self, params), fields(sql = %where_clause))]
    pub async fn legacy_lustre_count_where(&self, where_clause: &str, params: &[QueryParam]) -> Result<u64> {
        let sql = format!("SELECT COUNT(*) AS c FROM entries WHERE {where_clause}");
        let mut query = sqlx::query(&sql);
        for p in params {
            query = match p {
                QueryParam::Int(n) => query.bind(*n),
                QueryParam::Str(s) => query.bind(s.as_str()),
            };
        }
        let row = query.fetch_one(&self.pool).await?;
        let c: i64 = row.try_get("c")?;
        Ok(c as u64)
    }

    /// `SUM(size)` across all rows matching the predicate. Returns 0 when
    /// nothing matches. Used by threshold triggers (fire condition) and by
    /// the low-watermark in-run stopper.
    #[tracing::instrument(name = "store.legacy_lustre_sum_size_where", skip(self, params), fields(sql = %where_clause))]
    pub async fn legacy_lustre_sum_size_where(&self, where_clause: &str, params: &[QueryParam]) -> Result<u64> {
        let sql = format!(
            "SELECT CAST(COALESCE(SUM(size), 0) AS UNSIGNED) AS total \
             FROM entries WHERE {where_clause}"
        );
        let mut query = sqlx::query(&sql);
        for p in params {
            query = match p {
                QueryParam::Int(n) => query.bind(*n),
                QueryParam::Str(s) => query.bind(s.as_str()),
            };
        }
        let row = query.fetch_one(&self.pool).await?;
        Ok(row.try_get::<u64, _>("total").unwrap_or(0))
    }

    /// Sweep entries whose `last_seen < before` into `removed_entries`.
    ///
    /// Covers the case where a file was deleted out-of-band while the
    /// daemon was down and the changelog ring overflowed: a full scan
    /// refreshes `last_seen` on everything currently on disk, so anything
    /// still stale after the scan completed is presumed gone.
    ///
    /// Atomically update xattr tags in `sm_status.xattr`.
    ///
    /// Clears every key in `clear_keys`, then sets each key in `tags`.
    /// Other fields in `sm_status` (e.g. `hsm_state`) are untouched.
    #[tracing::instrument(name = "store.legacy_lustre_update_xattr", skip(self, tags, clear_keys), fields(fid = %fid))]
    pub async fn legacy_lustre_update_xattr(
        &self, fid: &LuFid, tags: &std::collections::HashMap<String, String>, clear_keys: &[String],
    ) -> Result<()> {
        if tags.is_empty() && clear_keys.is_empty() {
            return Ok(());
        }
        let fid_bin = fid_codec::encode(fid);

        // Read current sm_status, patch in Rust, write back atomically.
        // This avoids complex nested JSON_SET SQL that varies by row count.
        let row = sqlx::query("SELECT sm_status FROM entries WHERE fid = ?")
            .bind(fid_bin.as_slice())
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(());
        };

        let sm_bytes: Option<Vec<u8>> = row.try_get("sm_status")?;
        let mut sm: serde_json::Value = match sm_bytes {
            Some(b) if !b.is_empty() => serde_json::from_slice(&b)?,
            _ => serde_json::json!({}),
        };

        // Ensure sm_status.xattr sub-object exists
        if sm.get("xattr").is_none() {
            sm["xattr"] = serde_json::json!({});
        }
        let xattr = sm["xattr"].as_object_mut().expect("xattr is object");

        for key in clear_keys {
            xattr.remove(key);
        }
        for (k, v) in tags {
            xattr.insert(k.clone(), serde_json::Value::String(v.clone()));
        }

        let sm_json = serde_json::to_string(&sm)?;
        sqlx::query("UPDATE entries SET sm_status = ? WHERE fid = ?")
            .bind(&sm_json)
            .bind(fid_bin.as_slice())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Never sweeps directories (they're handled by the scanner
    /// distinctly) and caps work at `limit` rows per call. Call
    /// repeatedly until the returned count is zero.
    ///
    /// When `dry_run` is true, the scan counts candidates but performs
    /// no deletes.
    #[tracing::instrument(name = "store.legacy_lustre_sweep_orphans", skip(self))]
    pub async fn legacy_lustre_sweep_orphans(&self, before: i64, limit: u64, dry_run: bool) -> Result<u64> {
        // Candidate fids, ordered oldest-first so repeated calls make
        // monotonic progress.
        let sql = "SELECT fid FROM entries WHERE last_seen < ? AND kind != 1 \
                   ORDER BY last_seen ASC LIMIT ?";
        let fids: Vec<Vec<u8>> = sqlx::query_scalar(sql)
            .bind(before)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
        if fids.is_empty() {
            return Ok(0);
        }
        if dry_run {
            return Ok(fids.len() as u64);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut swept = 0u64;
        for fid_bin in fids {
            // Decode the blob back into a LuFid to reuse `legacy_lustre_remove_entry`'s
            // transactional move. We could do this with a single bulk
            // INSERT ... SELECT / DELETE pair, but per-row keeps the
            // existing semantics (names-table cleanup included) without
            // duplicating that SQL here.
            if let Some(fid) = fid_codec::decode(&fid_bin) {
                if let Err(e) = self.legacy_lustre_remove_entry(&fid, now).await {
                    tracing::warn!(fid = %fid, error = %e, "legacy_lustre_sweep_orphans: legacy_lustre_remove_entry failed");
                    continue;
                }
                swept += 1;
            }
        }
        Ok(swept)
    }

    /// Totals (`file_count`, `total_bytes`) across the FID subtree
    /// rooted at `root`. Implemented with a MariaDB recursive CTE on the
    /// `parent_fid` edge. Includes the root itself in the totals when
    /// it exists. Returns `(0, 0)` when the root is absent.
    #[tracing::instrument(name = "store.legacy_lustre_subtree_totals", skip(self), fields(fid = %root))]
    pub async fn legacy_lustre_subtree_totals(&self, root: &LuFid) -> Result<(u64, u64)> {
        let root_bin = fid_codec::encode(root);
        let sql = "WITH RECURSIVE descendants (fid) AS ( \
                     SELECT fid FROM entries WHERE fid = ? \
                     UNION ALL \
                     SELECT e.fid FROM entries e \
                     JOIN descendants d ON e.parent_fid = d.fid \
                   ) \
                   SELECT COUNT(*) AS n, \
                          CAST(COALESCE(SUM(size), 0) AS UNSIGNED) AS bytes \
                   FROM entries WHERE fid IN (SELECT fid FROM descendants)";
        let row = sqlx::query(sql).bind(root_bin.as_slice()).fetch_one(&self.pool).await?;
        let count = row.try_get::<i64, _>("n")? as u64;
        let bytes = row.try_get::<u64, _>("bytes").unwrap_or(0);
        Ok((count, bytes))
    }

    /// Per-OST stripe distribution: for each OST index that appears in
    /// `stripe_items`, return `(ost_index, file_count, approx_bytes)`.
    ///
    /// `approx_bytes` is `SUM(size / stripe_count)` — the per-OST share
    /// of file bytes under the assumption that Lustre splits each file
    /// evenly across its stripes. Files with `stripe_count = 0` (not a
    /// striped file, or metadata missing) are skipped via NULLIF.
    ///
    /// Ordered by `approx_bytes DESC` so "fullest OST first" is the
    /// default view.
    #[tracing::instrument(name = "store.legacy_lustre_stripe_distribution", skip(self))]
    pub async fn legacy_lustre_stripe_distribution(&self) -> Result<Vec<StripeDistRow>> {
        let sql = "SELECT si.ost_index, COUNT(*) AS n, \
                   CAST(COALESCE(SUM(e.size / NULLIF(e.stripe_count, 0)), 0) AS UNSIGNED) AS bytes \
                   FROM stripe_items si \
                   JOIN entries e ON e.fid = si.fid \
                   GROUP BY si.ost_index \
                   ORDER BY bytes DESC";
        let rows = sqlx::query(sql).fetch_all(&self.pool).await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push(StripeDistRow {
                ost_index: row.try_get::<u32, _>("ost_index")?,
                file_count: row.try_get::<i64, _>("n")? as u64,
                approx_bytes: row.try_get::<u64, _>("bytes").unwrap_or(0),
            });
        }
        Ok(out)
    }

    /// Fetch one page of entries ordered by FID, strictly greater than
    /// `after`. Used by the catalog dump to stream the whole table in
    /// deterministic, resumable chunks without holding a long-running
    /// server-side cursor (MariaDB doesn't expose one to sqlx).
    ///
    /// Returns at most `limit` rows; callers continue with
    /// `legacy_lustre_dump_page(rows.last().map(|r| r.fid), limit)` until the result
    /// is shorter than `limit`.
    #[tracing::instrument(name = "store.legacy_lustre_dump_page", skip(self))]
    pub async fn legacy_lustre_dump_page(&self, after: Option<LuFid>, limit: u64) -> Result<Vec<EntryRow>> {
        let sql = "SELECT fid, parent_fid, name, kind, size, blocks, uid, gid, projid, \
                   mode, nlink, atime, mtime, ctime, stripe_count, stripe_size, \
                   pool_name, sm_status, last_seen, depth \
                   FROM entries WHERE fid > ? ORDER BY fid LIMIT ?";
        let lo = after.unwrap_or(LuFid::ZERO);
        let lo_bin = crate::fid_codec::encode(&lo);
        let rows = sqlx::query(sql)
            .bind(lo_bin.as_slice())
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(row_to_entry).collect()
    }
}

async fn upsert_scoped_entry_tx(tx: &mut sqlx::Transaction<'_, MySql>, entry: &ScopedEntryRow) -> Result<()> {
    let (object_kind, object_id) = encode_object_id(*entry.key.object());
    let (parent_kind, parent_id) = match entry.parent.as_ref() {
        Some(parent) => {
            if parent.filesystem() != entry.key.filesystem() {
                return Err(StoreError::InvalidObjectIdentity(
                    "entry parent belongs to another filesystem",
                ));
            }
            let (kind, id) = encode_object_id(*parent.object());
            (Some(kind), Some(id))
        }
        None => (None, None),
    };
    let fid = match entry.key.object() {
        ObjectId::Lustre(fid) => Some(fid_codec::encode(fid)),
        ObjectId::JuiceFs(_) => None,
    };
    let entry_data = serde_json::to_string(entry)?;
    let sm_status = serde_json::to_string(&entry.sm_status)?;
    sqlx::query(
        r"INSERT INTO scoped_entries
            (filesystem_id, object_kind, object_id, entry_data, parent_kind, parent_id, fid,
             name, kind, size, blocks, uid, gid, projid, mode, nlink, atime, mtime, ctime,
             stripe_count, stripe_size, pool_name, sm_status, last_seen, depth)
          VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
          ON DUPLICATE KEY UPDATE
            entry_data = VALUES(entry_data), parent_kind = VALUES(parent_kind), parent_id = VALUES(parent_id),
            fid = VALUES(fid), name = VALUES(name), kind = VALUES(kind), size = VALUES(size),
            blocks = VALUES(blocks), uid = VALUES(uid), gid = VALUES(gid), projid = VALUES(projid),
            mode = VALUES(mode), nlink = VALUES(nlink), atime = VALUES(atime), mtime = VALUES(mtime),
            ctime = VALUES(ctime), stripe_count = VALUES(stripe_count), stripe_size = VALUES(stripe_size),
            pool_name = VALUES(pool_name), sm_status = VALUES(sm_status), last_seen = VALUES(last_seen),
            depth = VALUES(depth)",
    )
    .bind(entry.key.filesystem().as_str())
    .bind(object_kind)
    .bind(object_id.as_slice())
    .bind(entry_data)
    .bind(parent_kind)
    .bind(parent_id.as_ref().map(|id| id.as_slice()))
    .bind(fid.as_ref().map(|id| id.as_slice()))
    .bind(entry.name.as_ref())
    .bind(entry.kind as u8)
    .bind(entry.size)
    .bind(entry.blocks)
    .bind(entry.uid)
    .bind(entry.gid)
    .bind(entry.projid)
    .bind(entry.mode)
    .bind(entry.nlink)
    .bind(entry.atime)
    .bind(entry.mtime)
    .bind(entry.ctime)
    .bind(entry.stripe_count)
    .bind(entry.stripe_size)
    .bind(&entry.pool_name)
    .bind(sm_status)
    .bind(entry.last_seen)
    .bind(entry.depth)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

// ── MariaDB CursorStore implementation ──────────────────────────────────

/// MariaDB-backed implementation of `lustre_changelog::CursorStore`.
///
/// Stores the last committed record index per MDT in the `changelog_cursor`
/// table. Uses `INSERT ... ON DUPLICATE KEY UPDATE` for atomicity.
pub struct MariaDbCursorStore {
    pool: Pool<MySql>,
}

impl MariaDbCursorStore {
    pub fn new(pool: Pool<MySql>) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl lustre_changelog::CursorStore for MariaDbCursorStore {
    async fn get(&self, mdt: &str) -> std::result::Result<Option<u64>, lustre_changelog::CursorError> {
        let row = sqlx::query("SELECT last_rec FROM changelog_cursor WHERE mdt_name = ?")
            .bind(mdt)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| lustre_changelog::CursorError::Backend { message: e.to_string() })?;

        match row {
            Some(r) => {
                let v: u64 = r
                    .try_get("last_rec")
                    .map_err(|e| lustre_changelog::CursorError::Backend { message: e.to_string() })?;
                Ok(Some(v))
            }
            None => Ok(None),
        }
    }

    async fn commit(&self, mdt: &str, rec_id: u64) -> std::result::Result<(), lustre_changelog::CursorError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // Use GREATEST to enforce monotonicity at the DB level.
        sqlx::query(
            "INSERT INTO changelog_cursor (mdt_name, last_rec, updated_at) VALUES (?, ?, ?)
             ON DUPLICATE KEY UPDATE last_rec = GREATEST(last_rec, VALUES(last_rec)), updated_at = VALUES(updated_at)",
        )
        .bind(mdt)
        .bind(rec_id)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| lustre_changelog::CursorError::Backend { message: e.to_string() })?;

        Ok(())
    }
}

// ── Row conversion helpers ──────────────────────────────────────────────

fn row_to_entry(row: &sqlx::mysql::MySqlRow) -> Result<EntryRow> {
    let fid_bytes: Vec<u8> = row.try_get("fid")?;
    let fid = fid_codec::decode(&fid_bytes).ok_or(StoreError::FidCodec("invalid fid in entries table"))?;

    let parent_bytes: Option<Vec<u8>> = row.try_get("parent_fid")?;
    let parent_fid = match parent_bytes {
        Some(b) if !b.is_empty() => {
            Some(fid_codec::decode(&b).ok_or(StoreError::FidCodec("corrupted parent_fid in entries table"))?)
        }
        _ => None,
    };

    let name_bytes: Vec<u8> = row.try_get("name")?;
    let kind_u8: u8 = row.try_get("kind")?;
    let kind = EntryKind::from_u8(kind_u8).ok_or(StoreError::FidCodec("invalid entry kind in entries table"))?;

    // MariaDB JSON column is LONGTEXT/BLOB; sqlx decodes as Vec<u8>.
    // H2 fix: propagate JSON parse errors instead of silent Null fallback.
    let sm_bytes: Option<Vec<u8>> = row.try_get("sm_status")?;
    let sm_status: serde_json::Value = match sm_bytes {
        Some(b) if !b.is_empty() => serde_json::from_slice(&b)?,
        _ => serde_json::Value::Null,
    };

    Ok(EntryRow {
        fid,
        parent_fid,
        name: bytes::Bytes::from(name_bytes),
        kind,
        size: row.try_get::<u64, _>("size")?,
        blocks: row.try_get::<u64, _>("blocks")?,
        uid: row.try_get::<u32, _>("uid")?,
        gid: row.try_get::<u32, _>("gid")?,
        projid: row.try_get::<u32, _>("projid")?,
        mode: row.try_get::<u32, _>("mode")?,
        nlink: row.try_get::<u32, _>("nlink")?,
        atime: row.try_get("atime")?,
        mtime: row.try_get("mtime")?,
        ctime: row.try_get("ctime")?,
        stripe_count: row.try_get("stripe_count")?,
        stripe_size: row.try_get("stripe_size")?,
        stripe_items: Vec::new(),
        pool_name: row.try_get("pool_name")?,
        sm_status,
        last_seen: row.try_get("last_seen")?,
        depth: row.try_get::<u32, _>("depth").unwrap_or(0),
    })
}

fn encode_object_id(object: ObjectId) -> (u8, [u8; 16]) {
    match object {
        ObjectId::Lustre(fid) => (0, fid_codec::encode(&fid)),
        ObjectId::JuiceFs(inode) => {
            let mut bytes = [0; 16];
            bytes[8..].copy_from_slice(&inode.to_be_bytes());
            (1, bytes)
        }
    }
}

fn decode_object_id(kind: u8, bytes: &[u8]) -> Result<ObjectId> {
    match kind {
        0 => fid_codec::decode(bytes)
            .map(ObjectId::Lustre)
            .ok_or(StoreError::InvalidObjectIdentity("invalid Lustre object id")),
        1 if bytes.len() == 16 => Ok(ObjectId::JuiceFs(u64::from_be_bytes(
            bytes[8..].try_into().expect("length checked"),
        ))),
        _ => Err(StoreError::InvalidObjectIdentity("invalid scoped object id")),
    }
}

fn row_to_filesystem(row: &sqlx::mysql::MySqlRow) -> Result<FileSystemConfig> {
    let id_text: String = row.try_get("id")?;
    let id = FileSystemId::new(id_text)?;
    let backend_text: String = row.try_get("backend_kind")?;
    let mount_path: Vec<u8> = row.try_get("mount_path")?;
    let capabilities: Vec<u8> = row.try_get("capabilities")?;

    Ok(FileSystemConfig {
        id,
        backend: crate::model::BackendKind::from_persisted(&backend_text)?,
        mount_path: OsString::from_vec(mount_path).into(),
        capabilities: serde_json::from_slice(&capabilities)?,
    })
}

fn row_to_removed(row: &sqlx::mysql::MySqlRow) -> Result<crate::model::RemovedEntry> {
    let fid_bytes: Vec<u8> = row.try_get("fid")?;
    let fid = fid_codec::decode(&fid_bytes).ok_or(StoreError::FidCodec("invalid fid in removed_entries table"))?;

    let parent_bytes: Option<Vec<u8>> = row.try_get("parent_fid")?;
    let parent_fid = match parent_bytes {
        Some(b) if !b.is_empty() => {
            Some(fid_codec::decode(&b).ok_or(StoreError::FidCodec("corrupted parent_fid in removed_entries"))?)
        }
        _ => None,
    };

    let name_bytes: Vec<u8> = row.try_get("name")?;
    let kind_u8: u8 = row.try_get("kind")?;
    let kind = EntryKind::from_u8(kind_u8).ok_or(StoreError::FidCodec("invalid entry kind in removed_entries"))?;

    let sm_bytes: Option<Vec<u8>> = row.try_get("sm_status")?;
    let sm_status: serde_json::Value = match sm_bytes {
        Some(b) if !b.is_empty() => serde_json::from_slice(&b)?,
        _ => serde_json::Value::Null,
    };

    Ok(crate::model::RemovedEntry {
        fid,
        parent_fid,
        name: bytes::Bytes::from(name_bytes),
        kind,
        size: row.try_get::<u64, _>("size")?,
        uid: row.try_get::<u32, _>("uid")?,
        gid: row.try_get::<u32, _>("gid")?,
        sm_status,
        rm_time: row.try_get("rm_time")?,
    })
}
