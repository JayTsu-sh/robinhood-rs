//! Full operation aggregates and a consistent read view for downloadable reports.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{MySql, MySqlConnection, MySqlExecutor, QueryBuilder, Transaction};

use super::{
    HistoryOperation, OperationFilter, OperationState, PolicyStore, RunHistory, operation_rows, pinned_run,
    read_snapshot, run_exists,
};
use crate::{PolicyError, store::Rev8PolicyRow};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationResults {
    matched_count: u64,
    pub pending_count: u64,
    completed_logical_bytes: String,
    by_state: BTreeMap<OperationState, u64>,
}

impl Default for OperationResults {
    fn default() -> Self {
        Self {
            matched_count: 0,
            pending_count: 0,
            completed_logical_bytes: "0".into(),
            by_state: BTreeMap::new(),
        }
    }
}

impl OperationResults {
    fn add(&mut self, group: &OperationGroup) {
        self.matched_count += group.count;
        *self.by_state.entry(group.state).or_default() += group.count;
        match group.state {
            OperationState::Submitting | OperationState::Submitted | OperationState::Unknown => {
                self.pending_count += group.count;
            }
            OperationState::Completed => self.completed_logical_bytes.clone_from(&group.logical_bytes),
            _ => {}
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationSummary {
    #[serde(flatten)]
    results: OperationResults,
    by_reason: Vec<OperationReason>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OperationReason {
    reason: Option<String>,
    count: u64,
    by_state: BTreeMap<OperationState, u64>,
}

#[derive(sqlx::FromRow)]
struct OperationGroup {
    run_id: u64,
    #[sqlx(try_from = "String")]
    state: OperationState,
    count: u64,
    logical_bytes: String,
    #[sqlx(default)]
    reason: Option<String>,
}

const EXCEPTION_REASON: &str = "CASE WHEN o.state IN ('failed','deferred','unknown') THEN o.reason END";
const GROUP_TOTALS: &str = "o.run_id, CAST(o.state AS CHAR) AS state, CAST(COUNT(*) AS UNSIGNED) AS count,
    CAST(SUM(CASE WHEN o.state = 'completed' THEN o.size ELSE 0 END) AS CHAR) AS logical_bytes";

async fn operation_summary(
    executor: impl MySqlExecutor<'_>, run: u64, filter: &OperationFilter,
) -> Result<OperationSummary, PolicyError> {
    let mut query = QueryBuilder::<MySql>::new(format!(
        "SELECT {GROUP_TOTALS}, MIN({EXCEPTION_REASON}) AS reason FROM rev8_operations o WHERE o.run_id = "
    ));
    query.push_bind(run);
    filter.constrain(&mut query);
    // Collation must not merge distinct recorded reason strings.
    query.push(format!(" GROUP BY o.run_id, o.state, BINARY ({EXCEPTION_REASON})"));
    let groups = query.build_query_as::<OperationGroup>().fetch_all(executor).await?;
    let mut results = OperationResults::default();
    let mut reasons = BTreeMap::new();
    for group in groups {
        results.add(&group);
        if group.state.is_exception() {
            let reason = reasons.entry(group.reason.clone()).or_insert(OperationReason {
                reason: group.reason,
                count: 0,
                by_state: BTreeMap::new(),
            });
            reason.count += group.count;
            reason.by_state.insert(group.state, group.count);
        }
    }
    let by_reason = reasons.into_values().collect();
    Ok(OperationSummary { results, by_reason })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunReportSnapshot {
    pub generated_at: DateTime<Utc>,
    pub run: RunHistory,
    pub policy_revision: Rev8PolicyRow,
    pub operation_results: OperationSummary,
}

pub struct ReportOperationPages {
    tx: Transaction<'static, MySql>,
    run: u64,
    filter: OperationFilter,
    after: Option<u64>,
}

impl ReportOperationPages {
    #[tracing::instrument(skip(self), fields(run_id = self.run))]
    pub async fn next_page(&mut self) -> Result<Vec<HistoryOperation>, PolicyError> {
        let page = operation_rows(self.run, &self.filter, self.after, 100)
            .build_query_as::<HistoryOperation>()
            .fetch_all(&mut *self.tx)
            .await?;
        if let Some(last) = page.last() {
            self.after = Some(last.operation.id);
        }
        Ok(page)
    }

    #[tracing::instrument(skip(self), fields(run_id = self.run))]
    pub async fn finish(self) -> Result<(), PolicyError> {
        Ok(self.tx.commit().await?)
    }
}

pub(super) async fn populate_run_results(
    connection: &mut MySqlConnection, runs: &mut [RunHistory],
) -> Result<(), PolicyError> {
    if runs.is_empty() {
        return Ok(());
    }
    let mut query = QueryBuilder::<MySql>::new(format!(
        "SELECT {GROUP_TOTALS} FROM rev8_operations o WHERE o.run_id IN ("
    ));
    let mut run_ids = query.separated(", ");
    for run in runs.iter() {
        run_ids.push_bind(run.run.id);
    }
    query.push(") GROUP BY o.run_id, o.state");
    let groups = query.build_query_as::<OperationGroup>().fetch_all(connection).await?;
    let mut results = BTreeMap::<_, OperationResults>::new();
    for group in groups {
        results.entry(group.run_id).or_default().add(&group);
    }
    for run in runs {
        run.record_results(results.remove(&run.run.id).unwrap_or_default());
    }
    Ok(())
}

impl PolicyStore {
    #[tracing::instrument(skip(self))]
    pub async fn summarize_rev8_operations(
        &self, run: u64, filter: OperationFilter,
    ) -> Result<Option<OperationSummary>, PolicyError> {
        if !run_exists(run, &self.pool).await? {
            return Ok(None);
        }
        Ok(Some(operation_summary(&self.pool, run, &filter).await?))
    }

    #[tracing::instrument(skip(self))]
    pub async fn open_rev8_run_report(
        &self, id: u64, filter: OperationFilter,
    ) -> Result<Option<(RunReportSnapshot, ReportOperationPages)>, PolicyError> {
        let mut tx = read_snapshot(&self.pool).await?;
        let generated_at = Utc::now();
        let Some(run) = pinned_run(id, &mut *tx).await? else {
            return Ok(None);
        };
        let policy_revision = Self::read_rev8_revision(run.run.policy_id, run.run.revision, &mut *tx).await?;
        let operation_results = operation_summary(&mut *tx, id, &filter).await?;
        Ok(Some((
            RunReportSnapshot {
                generated_at,
                run,
                policy_revision,
                operation_results,
            },
            ReportOperationPages {
                tx,
                run: id,
                filter,
                after: None,
            },
        )))
    }
}
