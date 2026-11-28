use std::{io, time::Duration};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::stream;
use lustre_api::LuFid;
use rbh_policy::store::{
    HistoryOperation, HistoryPage, OperationFilter, OperationSearch, OperationState, OperationSummary, RunHistory,
    RunReportSnapshot, RunSearch, RunState,
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
        .route("/runs/{id}", get(get_run))
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
    before_id: Option<u64>,
    limit: Option<u16>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OperationQuery {
    state: Option<OperationState>,
    reason: Option<String>,
    fid: Option<String>,
    #[serde(default)]
    exceptions_only: bool,
    after_id: Option<u64>,
    limit: Option<u16>,
}

impl OperationQuery {
    fn filter(self) -> Result<OperationFilter, ApiError> {
        Ok(OperationFilter {
            state: self.state,
            reason: self.reason.filter(|reason| !reason.is_empty()),
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
        before_id: query.before_id,
        limit: page_limit(query.limit)?,
    };
    Ok(Json(state.policy_store.search_policy_runs(filter).await?))
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
