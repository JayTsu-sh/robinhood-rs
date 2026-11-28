use std::{io, time::Duration};

use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{
        HeaderMap, StatusCode,
        header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::stream;
use lustre_api::LuFid;
use rbh_policy::store::{
    HistoryOperation, HistoryPage, OperationFilter, OperationSearch, OperationState, OperationSummary,
    RunActivationMode, RunControl, RunHistory, RunReportSnapshot, RunSearch, RunState,
};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{Semaphore, mpsc},
    time::{Instant, timeout_at},
};

use crate::{AppState, handlers::ApiError};

// Report producers reserve at most two snapshot connections from the daemon's pool.
static REPORT_GENERATORS: Semaphore = Semaphore::const_new(2);
const REPORT_GENERATION_TIMEOUT: Duration = Duration::from_secs(300);
const REPORT_SEND_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/runs", get(list_runs))
        .route("/runs/summary", get(run_statistics))
        .route("/runs/{id}", get(get_run))
        .route("/runs/{id}/control", post(control_run))
        .route("/runs/{id}/telemetry", get(run_telemetry))
        .route("/runs/{id}/operations", get(list_operations))
        .route("/runs/{id}/operations/summary", get(summarize_operations))
        .route("/runs/{id}/reports/{kind}", get(export_report))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunQuery {
    policy_id: Option<u64>,
    policy_name: Option<String>,
    run_id: Option<u64>,
    state: Option<RunState>,
    activation_mode: Option<RunActivationMode>,
    before_id: Option<u64>,
    limit: Option<u16>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OperationQuery {
    state: Option<OperationState>,
    reason: Option<String>,
    fid: Option<String>,
    path: Option<String>,
    #[serde(default)]
    exceptions_only: bool,
    after_id: Option<u64>,
    limit: Option<u16>,
}

impl OperationQuery {
    fn filter(self) -> Result<OperationFilter, ApiError> {
        let path = trimmed(self.path);
        if path.as_ref().is_some_and(|path| path.len() > 4096) {
            return Err(ApiError::BadRequest("path search exceeds 4096 bytes".into()));
        }
        Ok(OperationFilter {
            state: self.state,
            reason: self.reason.filter(|reason| !reason.is_empty()),
            path,
            exceptions_only: self.exceptions_only,
            fid: trimmed(self.fid)
                .map(|value| value.parse::<LuFid>())
                .transpose()
                .map_err(|error| ApiError::BadRequest(error.to_string()))?,
        })
    }
}

fn page_limit(limit: Option<u16>) -> Result<u16, ApiError> {
    let limit = limit.unwrap_or(25);
    if !(1..=100).contains(&limit) {
        return Err(ApiError::BadRequest("limit must be between 1 and 100".into()));
    }
    Ok(limit)
}

fn trimmed(value: Option<String>) -> Option<String> {
    value.map(|text| text.trim().to_owned()).filter(|text| !text.is_empty())
}

#[tracing::instrument(skip(state))]
async fn list_runs(
    State(state): State<AppState>, Query(query): Query<RunQuery>,
) -> Result<Json<HistoryPage<RunHistory>>, ApiError> {
    let filter = RunSearch {
        policy_id: query.policy_id,
        policy_name: trimmed(query.policy_name),
        run_id: query.run_id,
        state: query.state,
        activation_mode: query.activation_mode,
        before_id: query.before_id,
        limit: page_limit(query.limit)?,
    };
    Ok(Json(state.policy_store.search_policy_runs(filter).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatisticsQuery {
    policy_id: Option<u64>,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
struct RunStatistics {
    total: u64,
    queued: u64,
    running: u64,
    paused: u64,
    awaiting_approval: u64,
    ended: u64,
    exception_runs: u64,
    completed_objects: u64,
    failed_objects: u64,
    completed_last_day: u64,
    completed_logical_bytes: String,
}

#[tracing::instrument(skip_all)]
async fn run_statistics(
    State(state): State<AppState>, Query(query): Query<StatisticsQuery>,
) -> Result<Json<RunStatistics>, ApiError> {
    let stats = sqlx::query_as(
        "WITH runs AS (SELECT id, state FROM policy_runs WHERE (? IS NULL OR policy_id = ?)),
           results AS (
             SELECT o.run_id, SUM(o.state = 'completed') AS completed, SUM(o.state = 'failed') AS failed,
               SUM(o.state IN ('failed','unknown','deferred')) AS exceptions,
               SUM(o.state = 'completed' AND o.finished_at >= UTC_TIMESTAMP(6) - INTERVAL 1 DAY) AS last_day,
               SUM(CASE WHEN o.state = 'completed' THEN o.size ELSE 0 END) AS logical_bytes
             FROM runs r JOIN policy_operations o ON o.run_id = r.id GROUP BY o.run_id
           )
           SELECT CAST(COUNT(*) AS UNSIGNED) AS total,
             CAST(COALESCE(SUM(state = 'queued'),0) AS UNSIGNED) AS queued,
             CAST(COALESCE(SUM(state = 'running'),0) AS UNSIGNED) AS running,
             CAST(COALESCE(SUM(state = 'paused'),0) AS UNSIGNED) AS paused,
             CAST(COALESCE(SUM(state = 'awaiting_approval'),0) AS UNSIGNED) AS awaiting_approval,
             CAST(COALESCE(SUM(state NOT IN ('queued','running','paused','awaiting_approval')),0) AS UNSIGNED) AS ended,
             CAST(COALESCE(SUM(exceptions > 0),0) AS UNSIGNED) AS exception_runs,
             CAST(COALESCE(SUM(completed),0) AS UNSIGNED) AS completed_objects,
             CAST(COALESCE(SUM(failed),0) AS UNSIGNED) AS failed_objects,
             CAST(COALESCE(SUM(last_day),0) AS UNSIGNED) AS completed_last_day,
             CAST(COALESCE(SUM(logical_bytes),0) AS CHAR) AS completed_logical_bytes
           FROM runs LEFT JOIN results ON results.run_id = runs.id",
    )
    .bind(query.policy_id)
    .bind(query.policy_id)
    .fetch_one(state.entry_store.pool())
    .await
    .map_err(|error| ApiError::Internal(format!("run statistics: {error}")))?;
    Ok(Json(stats))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RunTelemetry {
    controls: Vec<ControlEvent>,
    completions: Vec<CompletionSample>,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
struct ControlEvent {
    action: String,
    actor: String,
    occurred_at_millis: i64,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
struct CompletionSample {
    minute: i64,
    logical_bytes: String,
}

#[tracing::instrument(skip(state))]
async fn run_telemetry(State(state): State<AppState>, Path(id): Path<u64>) -> Result<Json<RunTelemetry>, ApiError> {
    let pool = state.entry_store.pool();
    let found = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM policy_runs WHERE id = ?)")
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(telemetry_error)?;
    if !found {
        return Err(run_not_found(id));
    }
    let controls = sqlx::query_as(
        "SELECT CAST(action AS CHAR) AS action, actor,
            TIMESTAMPDIFF(MICROSECOND, '1970-01-01', occurred_at) DIV 1000 AS occurred_at_millis FROM policy_run_controls
         WHERE run_id = ? ORDER BY id DESC LIMIT 100",
    ).bind(id).fetch_all(pool).await.map_err(telemetry_error)?;
    let completions = sqlx::query_as(
        "SELECT TIMESTAMPDIFF(MINUTE, '1970-01-01', finished_at) AS minute,
                CAST(SUM(size) AS CHAR) AS logical_bytes
         FROM policy_operations WHERE run_id = ? AND state = 'completed' AND finished_at IS NOT NULL
         GROUP BY minute ORDER BY minute DESC LIMIT 30",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .map_err(telemetry_error)?;
    Ok(Json(RunTelemetry { controls, completions }))
}

fn telemetry_error(error: sqlx::Error) -> ApiError {
    ApiError::Internal(format!("run telemetry: {error}"))
}

#[derive(Deserialize)]
struct RunCommand {
    action: RunControl,
}

#[tracing::instrument(skip_all, fields(id))]
async fn control_run(
    State(state): State<AppState>, Path(id): Path<u64>, Extension(writers): Extension<crate::PolicyWriters>,
    headers: HeaderMap, Json(command): Json<RunCommand>,
) -> Result<StatusCode, ApiError> {
    let actor = writers.authorize(&headers)?;
    state
        .policy_store
        .control_run(id, command.action, actor)
        .await
        .map_err(|error| match error {
            rbh_policy::PolicyError::NotFound(_) => run_not_found(id),
            error => error.into(),
        })?;
    Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(skip(state))]
async fn get_run(State(state): State<AppState>, Path(id): Path<u64>) -> Result<Json<RunHistory>, ApiError> {
    Ok(Json(
        state
            .policy_store
            .get_policy_run(id)
            .await?
            .ok_or_else(|| run_not_found(id))?,
    ))
}

#[tracing::instrument(skip(state))]
async fn list_operations(
    State(state): State<AppState>, Path(id): Path<u64>, Query(query): Query<OperationQuery>,
) -> Result<Json<HistoryPage<HistoryOperation>>, ApiError> {
    let filter = OperationSearch {
        after_id: query.after_id,
        limit: page_limit(query.limit)?,
        filter: query.filter()?,
    };
    Ok(Json(
        state
            .policy_store
            .search_run_operations(id, filter)
            .await?
            .ok_or_else(|| run_not_found(id))?,
    ))
}

#[tracing::instrument(skip(state))]
async fn summarize_operations(
    State(state): State<AppState>, Path(id): Path<u64>, Query(query): Query<OperationQuery>,
) -> Result<Json<OperationSummary>, ApiError> {
    Ok(Json(
        state
            .policy_store
            .summarize_run_operations(id, query.filter()?)
            .await?
            .ok_or_else(|| run_not_found(id))?,
    ))
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReportKind {
    Run,
    Exceptions,
}

#[derive(Serialize)]
struct ReportScope<'a> {
    coverage: &'static str,
    filters: &'a OperationFilter,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReportHeader<'a> {
    schema_version: u8,
    report_type: ReportKind,
    scope: ReportScope<'a>,
    #[serde(flatten)]
    snapshot: RunReportSnapshot,
}

#[tracing::instrument(skip(state))]
async fn export_report(
    State(state): State<AppState>, Path((id, kind)): Path<(u64, ReportKind)>, Query(query): Query<OperationQuery>,
) -> Result<Response, ApiError> {
    let mut filter = query.filter()?;
    if matches!(kind, ReportKind::Exceptions) {
        filter.exceptions_only = true;
    }
    let slot = REPORT_GENERATORS
        .try_acquire()
        .map_err(|_| ApiError::ServiceUnavailable("report generation is busy, retry shortly".into()))?;
    let deadline = Instant::now() + REPORT_GENERATION_TIMEOUT;
    let (snapshot, mut operations) = timeout_at(deadline, state.policy_store.open_run_report(id, filter.clone()))
        .await
        .map_err(|_| ApiError::ServiceUnavailable("report generation timed out".into()))??
        .ok_or_else(|| run_not_found(id))?;
    let mut chunk = serde_json::to_vec(&ReportHeader {
        schema_version: 1,
        report_type: kind,
        scope: ReportScope {
            coverage: "all_matching_operations",
            filters: &filter,
        },
        snapshot,
    })
    .map_err(|error| ApiError::Internal(format!("serialize report header: {error}")))?;
    chunk.pop();
    chunk.extend_from_slice(b",\"operations\":[");

    let (chunks, receiver) = mpsc::channel(2);
    // The producer has its own deadlines even when HTTP stops polling the body.
    let producer = tokio::spawn(async move {
        let _slot = slot;
        let write = async {
            send_report_chunk(&chunks, chunk).await?;
            let mut first = true;
            loop {
                let rows = operations.next_page().await.map_err(io::Error::other)?;
                if rows.is_empty() {
                    break;
                }
                let mut chunk = Vec::new();
                for row in rows {
                    if !first {
                        chunk.push(b',');
                    }
                    first = false;
                    serde_json::to_writer(&mut chunk, &row)?;
                }
                send_report_chunk(&chunks, chunk).await?;
            }
            operations.finish().await.map_err(io::Error::other)?;
            send_report_chunk(&chunks, b"]}\n".to_vec()).await
        };
        let result = tokio::select! {
            biased;
            () = chunks.closed() => Ok(()),
            result = timeout_at(deadline, write) => {
                result.unwrap_or_else(|error| Err(io::Error::new(io::ErrorKind::TimedOut, error)))
            }
        };
        if let Err(error) = &result {
            tracing::warn!(run_id = id, %error, "report download interrupted");
        }
        result
    });

    let filename = match kind {
        ReportKind::Run => format!("run-{id}.json"),
        ReportKind::Exceptions => format!("run-{id}-exceptions.json"),
    };
    let body = stream::try_unfold((receiver, producer), |(mut receiver, producer)| async move {
        if let Some(chunk) = receiver.recv().await {
            return Ok(Some((chunk, (receiver, producer))));
        }
        producer.await.map_err(io::Error::other)??;
        Ok::<_, io::Error>(None)
    });
    Ok((
        [
            (CONTENT_TYPE, "application/json".to_owned()),
            (CACHE_CONTROL, "no-store".to_owned()),
            (CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\"")),
        ],
        Body::from_stream(body),
    )
        .into_response())
}

async fn send_report_chunk(chunks: &mpsc::Sender<Vec<u8>>, chunk: Vec<u8>) -> io::Result<()> {
    chunks.send_timeout(chunk, REPORT_SEND_TIMEOUT).await.map_err(|error| {
        let kind = match error {
            mpsc::error::SendTimeoutError::Timeout(_) => io::ErrorKind::TimedOut,
            mpsc::error::SendTimeoutError::Closed(_) => io::ErrorKind::BrokenPipe,
        };
        io::Error::new(kind, "report download stopped reading")
    })
}

fn run_not_found(id: u64) -> ApiError {
    ApiError::NotFound(format!("run not found: id={id}"))
}
