//! Read projections of the run queue and its immutable policy revisions.

use chrono::{DateTime, Utc};
use lustre_api::LuFid;
use rbh_entry_store::fid_codec;
use serde::{Deserialize, Serialize, de::IntoDeserializer};
use sqlx::{MySql, MySqlPool, QueryBuilder, Transaction};

use super::{OPERATION_FIELDS, PolicyOperation, PolicyRun, PolicyStore, RUN_FIELDS};
use crate::PolicyError;
use crate::store::{POLICY_REVISION_FIELDS, PolicyRevision};

mod reports;
use reports::populate_run_results;
pub use reports::{OperationResults, OperationSummary, RunReportSnapshot};

const PINNED_RUN: &str = "FROM policy_runs r JOIN policy_revisions rev USING (policy_id, revision)";
const POLICY_NAME: &str = "CAST(JSON_VALUE(definition, '$.metadata.name') AS CHAR CHARACTER SET utf8mb4)";
const RUN_HISTORY_FIELDS: &str =
    "CAST(r.id AS CHAR) AS id_exact, CAST(r.policy_id AS CHAR) AS policy_id_exact,
    CAST(r.revision AS CHAR) AS revision_exact,
    IF(JSON_VALUE(rev.definition, '$.spec.execution.activation.mode') = 'enforced', 'enforced', 'preview') AS activation_mode,
    IF(JSON_VALUE(rev.definition, '$.spec.mode') = 'one_shot', 'one_shot', 'continuous') AS policy_mode,
    CAST(r.estimated_bytes AS CHAR) AS estimated_bytes_exact";
const POLICY_WITH_LATEST_RUN: &str =
    "CAST(latest.id AS CHAR) AS latest_id, CAST(latest.revision AS CHAR) AS latest_revision,
        latest.state AS latest_state, latest.started_at AS latest_started_at
     FROM policies p JOIN policy_revisions r ON r.policy_id = p.id AND r.revision = p.revision
     LEFT JOIN policy_runs latest ON latest.id = COALESCE(
        (SELECT id FROM policy_runs WHERE policy_id = p.id AND state = 'running' ORDER BY id DESC LIMIT 1),
        (SELECT id FROM policy_runs WHERE policy_id = p.id AND state = 'queued' ORDER BY id DESC LIMIT 1),
        (SELECT id FROM policy_runs WHERE policy_id = p.id ORDER BY id DESC LIMIT 1)
     ) WHERE p.name IS NOT NULL";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPage<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

impl<T> HistoryPage<T> {
    fn from_lookahead(mut items: Vec<T>, limit: u16, id: impl Fn(&T) -> u64) -> Self {
        let next_cursor = if items.len() > usize::from(limit) {
            items.truncate(usize::from(limit));
            items.last().map(|item| id(item).to_string())
        } else {
            None
        };
        Self { items, next_cursor }
    }
}

#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct RunHistory {
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub run: PolicyRun,
    pub id_exact: String,
    pub policy_id_exact: String,
    pub revision_exact: String,
    pub policy_name: Option<String>,
    pub activation_mode: String,
    pub policy_mode: String,
    pub estimated_bytes_exact: Option<String>,
    #[sqlx(skip)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_pending_operations: Option<bool>,
    #[sqlx(skip)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_results: Option<OperationResults>,
}

impl RunHistory {
    fn record_results(&mut self, results: OperationResults) {
        self.has_pending_operations = Some(results.pending_count > 0);
        self.operation_results = Some(results);
    }
}
#[derive(Debug, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Running,
    Previewed,
    Finished,
    Deferred,
    Cancelled,
}

#[derive(Debug)]
pub struct RunSearch {
    pub policy_id: Option<u64>,
    pub policy_name: Option<String>,
    pub run_id: Option<u64>,
    pub state: Option<RunState>,
    pub before_id: Option<u64>,
    pub limit: u16,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct HistoryOperation {
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub operation: PolicyOperation,
    pub id_exact: String,
    pub size_exact: String,
    pub finished_at: Option<DateTime<Utc>>,
    pub last_checked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum OperationState {
    Submitting,
    Submitted,
    Unknown,
    Completed,
    Failed,
    Deferred,
}

impl OperationState {
    fn is_exception(self) -> bool {
        matches!(self, Self::Failed | Self::Deferred | Self::Unknown)
    }
}

impl TryFrom<String> for OperationState {
    type Error = serde::de::value::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::deserialize(value.into_deserializer())
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationFilter {
    pub state: Option<OperationState>,
    #[serde(rename = "reasonContains")]
    pub reason: Option<String>,
    pub exceptions_only: bool,
    pub fid: Option<LuFid>,
}

impl OperationFilter {
    fn constrain<'a>(&'a self, query: &mut QueryBuilder<'a, MySql>) {
        if self.exceptions_only {
            query.push(" AND o.state IN ('failed','deferred','unknown')");
        }
        if let Some(state) = self.state {
            query.push(" AND o.state = ").push_bind(state);
        }
        if let Some(reason) = &self.reason {
            query.push(" AND LOCATE(").push_bind(reason).push(", o.reason) > 0");
        }
        if let Some(fid) = self.fid {
            query.push(" AND o.fid = ").push_bind(fid_codec::encode(&fid).to_vec());
        }
    }
}

#[derive(Debug)]
pub struct OperationSearch {
    pub filter: OperationFilter,
    pub after_id: Option<u64>,
    pub limit: u16,
}

fn operation_rows(run: u64, filter: &OperationFilter, after_id: Option<u64>, limit: u32) -> QueryBuilder<'_, MySql> {
    let mut query = QueryBuilder::new(format!(
        "SELECT {OPERATION_FIELDS}, CAST(o.id AS CHAR) AS id_exact, CAST(o.size AS CHAR) AS size_exact,
            o.finished_at, o.checked_at AS last_checked_at
         FROM policy_operations o JOIN policy_runs r ON r.id = o.run_id WHERE o.run_id = "
    ));
    query.push_bind(run);
    filter.constrain(&mut query);
    if let Some(after_id) = after_id {
        query.push(" AND o.id > ").push_bind(after_id);
    }
    query.push(" ORDER BY o.id LIMIT ").push_bind(limit);
    query
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestRun {
    pub id: String,
    pub revision: String,
    pub state: String,
    pub started_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct PolicyWithLatestRun {
    #[sqlx(flatten)]
    policy: PolicyRevision,
    latest_id: Option<String>,
    latest_revision: Option<String>,
    latest_state: Option<String>,
    latest_started_at: Option<DateTime<Utc>>,
}

impl PolicyWithLatestRun {
    fn into_policy(mut self) -> PolicyRevision {
        if let (Some(id), Some(revision), Some(state)) = (self.latest_id, self.latest_revision, self.latest_state) {
            self.policy.latest_run = Some(LatestRun {
                id,
                revision,
                state,
                started_at: self.latest_started_at,
            });
        }
        self.policy
    }
}

impl PolicyStore {
    #[tracing::instrument(skip(self))]
    pub async fn get_policy_with_latest_run(&self, id: u64) -> Result<Option<PolicyRevision>, PolicyError> {
        Ok(sqlx::query_as::<_, PolicyWithLatestRun>(&format!(
            "SELECT {POLICY_REVISION_FIELDS}, {POLICY_WITH_LATEST_RUN} AND p.id = ?"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .map(PolicyWithLatestRun::into_policy))
    }

    #[tracing::instrument(skip(self))]
    pub async fn list_policies_with_latest_run(
        &self, configured_only: bool,
    ) -> Result<Vec<PolicyRevision>, PolicyError> {
        let rows = sqlx::query_as::<_, PolicyWithLatestRun>(&format!(
            "SELECT {POLICY_REVISION_FIELDS}, {POLICY_WITH_LATEST_RUN} ORDER BY p.id"
        ))
        .fetch_all(&self.pool)
        .await?;
        let mut policies = rows.into_iter().map(PolicyWithLatestRun::into_policy).collect();
        if configured_only {
            self.retain_configured_policies(&mut policies);
        }
        Ok(policies)
    }

    #[tracing::instrument(skip(self))]
    pub async fn search_policy_runs(&self, filter: RunSearch) -> Result<HistoryPage<RunHistory>, PolicyError> {
        let mut query = QueryBuilder::<MySql>::new(format!(
            "SELECT {RUN_FIELDS}, {POLICY_NAME} AS policy_name, {RUN_HISTORY_FIELDS} {PINNED_RUN}"
        ));
        if let Some(policy_name) = filter.policy_name {
            // Match each immutable revision once even if it owns many runs.
            query
                .push(
                    " JOIN (SELECT DISTINCT policy_id AS matched_policy, revision AS matched_revision
                    FROM policy_revisions WHERE LOCATE(",
                )
                .push_bind(policy_name)
                .push(format!(", {POLICY_NAME}) > 0"));
            if let Some(policy_id) = filter.policy_id {
                query.push(" AND policy_id = ").push_bind(policy_id);
            }
            query
                .push(") matching ON matching.matched_policy = r.policy_id AND matching.matched_revision = r.revision");
        }
        query.push(" WHERE TRUE");
        if let Some(policy_id) = filter.policy_id {
            query.push(" AND r.policy_id = ").push_bind(policy_id);
        }
        if let Some(run_id) = filter.run_id {
            query.push(" AND r.id = ").push_bind(run_id);
        }
        if let Some(state) = filter.state {
            query.push(" AND r.state = ").push_bind(state);
        }
        if let Some(before_id) = filter.before_id {
            query.push(" AND r.id < ").push_bind(before_id);
        }
        query
            .push(" ORDER BY r.id DESC LIMIT ")
            .push_bind(u32::from(filter.limit) + 1);
        let mut tx = read_snapshot(&self.pool).await?;
        let rows = query.build_query_as().fetch_all(&mut *tx).await?;
        let mut page = HistoryPage::from_lookahead(rows, filter.limit, |row: &RunHistory| row.run.id);
        populate_run_results(&mut tx, &mut page.items).await?;
        tx.commit().await?;
        Ok(page)
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_policy_run(&self, id: u64) -> Result<Option<RunHistory>, PolicyError> {
        let mut tx = read_snapshot(&self.pool).await?;
        let mut run = pinned_run(id, &mut *tx).await?;
        populate_run_results(&mut tx, run.as_mut_slice()).await?;
        tx.commit().await?;
        Ok(run)
    }

    #[tracing::instrument(skip(self))]
    pub async fn search_run_operations(
        &self, run: u64, filter: OperationSearch,
    ) -> Result<Option<HistoryPage<HistoryOperation>>, PolicyError> {
        if !run_exists(run, &self.pool).await? {
            return Ok(None);
        }
        let mut query = operation_rows(run, &filter.filter, filter.after_id, u32::from(filter.limit) + 1);
        let rows = query.build_query_as().fetch_all(&self.pool).await?;
        Ok(Some(HistoryPage::from_lookahead(
            rows,
            filter.limit,
            |row: &HistoryOperation| row.operation.id,
        )))
    }
}

async fn pinned_run(id: u64, executor: impl sqlx::MySqlExecutor<'_>) -> Result<Option<RunHistory>, PolicyError> {
    Ok(sqlx::query_as(&format!(
        "SELECT {RUN_FIELDS}, {POLICY_NAME} AS policy_name, {RUN_HISTORY_FIELDS} {PINNED_RUN} WHERE r.id = ?"
    ))
    .bind(id)
    .fetch_optional(executor)
    .await?)
}

async fn run_exists(run: u64, pool: &sqlx::MySqlPool) -> Result<bool, PolicyError> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM policy_runs WHERE id = ?)")
            .bind(run)
            .fetch_one(pool)
            .await?,
    )
}

async fn read_snapshot(pool: &MySqlPool) -> Result<Transaction<'static, MySql>, PolicyError> {
    let mut connection = pool.acquire().await?;
    // A cancelled setup must not leave next-transaction settings on a pooled connection.
    connection.close_on_drop();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *connection)
        .await?;
    Ok(Transaction::begin(
        connection,
        Some("START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY".into()),
    )
    .await?)
}
