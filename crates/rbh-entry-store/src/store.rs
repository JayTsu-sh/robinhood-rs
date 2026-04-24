//! `EntryStore` — the main interface to the `rbh_entries` MariaDB database.

use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::mysql::MySqlPoolOptions;
use sqlx::{MySql, Pool, Row};
use tracing::{debug, info};

use lustre_api::LuFid;

use crate::error::{Result, StoreError};
use crate::fid_codec;
use crate::model::{EntryKind, EntryRow};

/// A bind parameter for `query_where`. Avoids circular dependency on `rbh-predicate`.
#[derive(Debug, Clone)]
pub enum QueryParam {
    Int(i64),
    Str(String),
}

/// Sort ordering for [`EntryStore::aggregate_by`].
#[derive(Debug, Clone, Copy)]
pub enum AggregateSort {
    Count,
    Size,
}

/// Whitelisted column names available for [`EntryStore::aggregate_by`].
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

/// One row returned by [`EntryStore::stripe_distribution`].
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

    // ── CRUD ────────────────────────────────────────────────────────────

    /// Insert or update a single entry via `INSERT ... ON DUPLICATE KEY UPDATE`.
    #[tracing::instrument(name = "store.upsert_entry", skip(self, entry), fields(fid = %entry.fid))]
    pub async fn upsert_entry(&self, entry: &EntryRow) -> Result<()> {
        let fid_bin = fid_codec::encode(&entry.fid);
        let parent_bin = entry.parent_fid.as_ref().map(fid_codec::encode);
        let sm_json = serde_json::to_string(&entry.sm_status)?;

        sqlx::query(
            r"INSERT INTO entries
                (fid, parent_fid, name, kind, size, blocks, uid, gid, projid, mode, nlink,
                 atime, mtime, ctime, stripe_count, stripe_size, pool_name, sm_status, last_seen)
              VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
                last_seen    = VALUES(last_seen)",
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
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get one entry by FID.
    #[tracing::instrument(name = "store.get_entry", skip(self), fields(fid = %fid))]
    pub async fn get_entry(&self, fid: &LuFid) -> Result<Option<EntryRow>> {
        let fid_bin = fid_codec::encode(fid);
        let row = sqlx::query(
            "SELECT fid, parent_fid, name, kind, size, blocks, uid, gid, projid, mode, nlink,
                    atime, mtime, ctime, stripe_count, stripe_size, pool_name, sm_status, last_seen
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
    #[tracing::instrument(name = "store.remove_entry", skip(self), fields(fid = %fid))]
    pub async fn remove_entry(&self, fid: &LuFid, rm_time: i64) -> Result<()> {
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
            tracing::warn!(fid = %fid, "remove_entry: FID not found in entries table");
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
    #[tracing::instrument(name = "store.upsert_batch", skip(self, entries), fields(count = entries.len()))]
    pub async fn upsert_batch(&self, entries: &[EntryRow]) -> Result<()> {
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
                     atime, mtime, ctime, stripe_count, stripe_size, pool_name, sm_status, last_seen)
                  VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
                    last_seen    = VALUES(last_seen)",
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
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        debug!(count = entries.len(), "batch upserted");
        Ok(())
    }

    /// Look up an entry by (parent_fid, name). Returns the FID if found.
    ///
    /// Used by changelog ingest to detect rename-overwrite: when a rename
    /// destination already exists, the displaced entry must be removed.
    #[tracing::instrument(name = "store.lookup_by_parent_name", skip(self))]
    pub async fn lookup_by_parent_name(&self, parent_fid: &LuFid, name: &[u8]) -> Result<Option<LuFid>> {
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
    pub async fn entry_count(&self) -> Result<u64> {
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
    #[tracing::instrument(name = "store.query_where", skip(self, params), fields(sql = %where_clause))]
    pub async fn query_where(&self, where_clause: &str, params: &[QueryParam], limit: u64) -> Result<Vec<EntryRow>> {
        self.query_page(where_clause, params, None, limit, 0).await
    }

    /// Paginated query with optional ORDER BY. `order_by` must be a
    /// pre-validated SQL fragment (column name + ASC/DESC). Callers build
    /// it via [`SortKey::to_sql_fragment`] — never from raw user input.
    pub async fn query_page(
        &self, where_clause: &str, params: &[QueryParam], order_by: Option<&str>, limit: u64, offset: u64,
    ) -> Result<Vec<EntryRow>> {
        let order_clause = match order_by {
            Some(o) if !o.is_empty() => format!(" ORDER BY {o}"),
            _ => String::new(),
        };
        let sql = format!(
            "SELECT fid, parent_fid, name, kind, size, blocks, uid, gid, projid, \
             mode, nlink, atime, mtime, ctime, stripe_count, stripe_size, \
             pool_name, sm_status, last_seen \
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
    pub async fn aggregate_by(
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
    pub async fn size_profile(&self) -> Result<Vec<(String, u64, u64)>> {
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
    pub async fn patch_sm_status(&self, fid: &LuFid, patch: &serde_json::Value) -> Result<bool> {
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
    pub async fn list_removed(
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
    pub async fn forget_removed(&self, fid: &LuFid) -> Result<bool> {
        let bytes = fid_codec::encode(fid);
        let res = sqlx::query("DELETE FROM removed_entries WHERE fid = ?")
            .bind(&bytes[..])
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Look up one removed entry by FID.
    pub async fn get_removed(&self, fid: &LuFid) -> Result<Option<crate::model::RemovedEntry>> {
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
    pub async fn count_where(&self, where_clause: &str, params: &[QueryParam]) -> Result<u64> {
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
    pub async fn sum_size_where(&self, where_clause: &str, params: &[QueryParam]) -> Result<u64> {
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
    /// Never sweeps directories (they're handled by the scanner
    /// distinctly) and caps work at `limit` rows per call. Call
    /// repeatedly until the returned count is zero.
    ///
    /// When `dry_run` is true, the scan counts candidates but performs
    /// no deletes.
    pub async fn sweep_orphans(&self, before: i64, limit: u64, dry_run: bool) -> Result<u64> {
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
            // Decode the blob back into a LuFid to reuse `remove_entry`'s
            // transactional move. We could do this with a single bulk
            // INSERT ... SELECT / DELETE pair, but per-row keeps the
            // existing semantics (names-table cleanup included) without
            // duplicating that SQL here.
            if let Some(fid) = fid_codec::decode(&fid_bin) {
                if let Err(e) = self.remove_entry(&fid, now).await {
                    tracing::warn!(fid = %fid, error = %e, "sweep_orphans: remove_entry failed");
                    continue;
                }
                swept += 1;
            }
        }
        Ok(swept)
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
    pub async fn stripe_distribution(&self) -> Result<Vec<StripeDistRow>> {
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
    /// `dump_page(rows.last().map(|r| r.fid), limit)` until the result
    /// is shorter than `limit`.
    pub async fn dump_page(&self, after: Option<LuFid>, limit: u64) -> Result<Vec<EntryRow>> {
        let sql = "SELECT fid, parent_fid, name, kind, size, blocks, uid, gid, projid, \
                   mode, nlink, atime, mtime, ctime, stripe_count, stripe_size, \
                   pool_name, sm_status, last_seen \
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
        pool_name: row.try_get("pool_name")?,
        sm_status,
        last_seen: row.try_get("last_seen")?,
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
