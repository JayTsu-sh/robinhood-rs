//! Read projections of the run queue and its immutable policy revisions.

use chrono::{DateTime, Utc};
use lustre_api::LuFid;
use rbh_entry_store::fid_codec;
use serde::{Deserialize, Serialize};
use sqlx::{MySql, QueryBuilder};

use super::{OPERATION_FIELDS, PolicyStore, RUN_FIELDS, Rev8Operation, Rev8Run};
use crate::PolicyError;
use crate::store::{REV8_POLICY_FIELDS, Rev8PolicyRow};

const PINNED_RUN: &str = "FROM rev8_runs r JOIN rev8_policy_revisions rev USING (policy_id, revision)";
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
     FROM policies p JOIN rev8_policy_revisions r ON r.policy_id = p.id AND r.revision = p.revision
     LEFT JOIN rev8_runs latest ON latest.id = COALESCE(
        (SELECT id FROM rev8_runs WHERE policy_id = p.id AND state = 'running' ORDER BY id DESC LIMIT 1),
        (SELECT id FROM rev8_runs WHERE policy_id = p.id AND state = 'queued' ORDER BY id DESC LIMIT 1),
        (SELECT id FROM rev8_runs WHERE policy_id = p.id ORDER BY id DESC LIMIT 1)
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
    pub run: Rev8Run,
    pub id_exact: String,
    pub policy_id_exact: String,
    pub revision_exact: String,
    pub policy_name: Option<String>,
    pub activation_mode: String,
    pub policy_mode: String,
    pub estimated_bytes_exact: Option<String>,
    #[sqlx(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_pending_operations: Option<bool>,
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
    pub operation: Rev8Operation,
    pub id_exact: String,
    pub size_exact: String,
    pub finished_at: Option<DateTime<Utc>>,
    pub last_checked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize, sqlx::Type)]
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

#[derive(Debug)]
pub struct OperationSearch {
    pub state: Option<OperationState>,
    pub reason: Option<String>,
    pub exceptions_only: bool,
    pub fid: Option<LuFid>,
    pub after_id: Option<u64>,
    pub limit: u16,
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
    policy: Rev8PolicyRow,
    latest_id: Option<String>,
    latest_revision: Option<String>,
    latest_state: Option<String>,
    latest_started_at: Option<DateTime<Utc>>,
}

impl PolicyWithLatestRun {
    fn into_policy(mut self) -> Rev8PolicyRow {
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
    pub async fn get_rev8_with_latest_run(&self, id: u64) -> Result<Option<Rev8PolicyRow>, PolicyError> {
        Ok(sqlx::query_as::<_, PolicyWithLatestRun>(&format!(
            "SELECT {REV8_POLICY_FIELDS}, {POLICY_WITH_LATEST_RUN} AND p.id = ?"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .map(PolicyWithLatestRun::into_policy))
    }

    #[tracing::instrument(skip(self))]
    pub async fn list_rev8_with_latest_run(&self, configured_only: bool) -> Result<Vec<Rev8PolicyRow>, PolicyError> {
        let rows = sqlx::query_as::<_, PolicyWithLatestRun>(&format!(
            "SELECT {REV8_POLICY_FIELDS}, {POLICY_WITH_LATEST_RUN} ORDER BY p.id"
        ))
        .fetch_all(&self.pool)
        .await?;
        let mut policies = rows.into_iter().map(PolicyWithLatestRun::into_policy).collect();
        if configured_only {
            self.retain_configured_rev8(&mut policies);
        }
        Ok(policies)
    }

    #[tracing::instrument(skip(self))]
    pub async fn search_rev8_runs(&self, filter: RunSearch) -> Result<HistoryPage<RunHistory>, PolicyError> {
        let mut query = QueryBuilder::<MySql>::new(format!(
            "SELECT {RUN_FIELDS}, {POLICY_NAME} AS policy_name, {RUN_HISTORY_FIELDS} {PINNED_RUN}"
        ));
        if let Some(policy_name) = filter.policy_name {
            // Match each immutable revision once even if it owns many runs.
            query
                .push(
                    " JOIN (SELECT DISTINCT policy_id AS matched_policy, revision AS matched_revision
                    FROM rev8_policy_revisions WHERE LOCATE(",
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
        let rows = query.build_query_as().fetch_all(&self.pool).await?;
        Ok(HistoryPage::from_lookahead(rows, filter.limit, |row: &RunHistory| {
            row.run.id
        }))
    }

    #[tracing::instrument(skip(self))]
    pub async fn rev8_run_detail(&self, id: u64) -> Result<Option<RunHistory>, PolicyError> {
        Ok(sqlx::query_as(&format!(
            "SELECT {RUN_FIELDS}, {POLICY_NAME} AS policy_name, {RUN_HISTORY_FIELDS},
                EXISTS(SELECT 1 FROM rev8_operations o WHERE o.run_id = r.id
                    AND o.state IN ('submitting','submitted','unknown')) AS has_pending_operations
             {PINNED_RUN} WHERE r.id = ?"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    #[tracing::instrument(skip(self))]
    pub async fn search_rev8_operations(
        &self, run: u64, filter: OperationSearch,
    ) -> Result<Option<HistoryPage<HistoryOperation>>, PolicyError> {
        let exists = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM rev8_runs WHERE id = ?)")
            .bind(run)
            .fetch_one(&self.pool)
            .await?;
        if !exists {
            return Ok(None);
        }

        let mut query = QueryBuilder::<MySql>::new(format!(
            "SELECT {OPERATION_FIELDS}, CAST(o.id AS CHAR) AS id_exact, CAST(o.size AS CHAR) AS size_exact,
                o.finished_at, o.checked_at AS last_checked_at
             FROM rev8_operations o JOIN rev8_runs r ON r.id = o.run_id WHERE o.run_id = "
        ));
        query.push_bind(run);
        if filter.exceptions_only {
            query.push(" AND o.state IN ('failed','deferred','unknown')");
        }
        if let Some(state) = filter.state {
            query.push(" AND o.state = ").push_bind(state);
        }
        if let Some(reason) = filter.reason {
            query.push(" AND LOCATE(").push_bind(reason).push(", o.reason) > 0");
        }
        if let Some(fid) = filter.fid {
            query.push(" AND o.fid = ").push_bind(fid_codec::encode(&fid).to_vec());
        }
        if let Some(after_id) = filter.after_id {
            query.push(" AND o.id > ").push_bind(after_id);
        }
        query
            .push(" ORDER BY o.id LIMIT ")
            .push_bind(u32::from(filter.limit) + 1);
        let rows = query.build_query_as().fetch_all(&self.pool).await?;
        Ok(Some(HistoryPage::from_lookahead(
            rows,
            filter.limit,
            |row: &HistoryOperation| row.operation.id,
        )))
    }
}
