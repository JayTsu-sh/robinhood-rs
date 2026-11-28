//! Sample and evaluate business-client read heat signals.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use chrono::{DateTime, TimeDelta, Utc};
use lustre_api::{
    LuFid,
    heat::{Decay, HeatMount, ReadHeat},
};
use rbh_entry_store::{BackendKind, EntryStore, FileSystemConfig, fid_codec};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, MySqlPool, QueryBuilder};
use tokio::sync::Mutex;

pub use crate::store::HeatSampler;

pub mod agent;
use agent::{AgentConfig, HeatFailure, MAX_BATCH_SIZE, SampleRequest, SampleResult, sample_mount};

use super::{CompiledPolicy, PreviewResult, invalid, resources::ResourceBinding, valid_name};
use crate::PolicyError;

const MISSING: &str = "heat_sample_missing";
const STALE: &str = "heat_sample_stale";
const MISMATCH: &str = "heat_decay_mismatch";
pub(crate) const NOT_CONFIGURED: &str = "heat_not_configured";
const CLIENT_UNAVAILABLE: &str = "heat_client_unavailable";

pub(crate) fn deferral_reason(status: Option<&str>) -> &'static str {
    match status {
        Some(STALE) => STALE,
        Some(MISMATCH) => MISMATCH,
        Some(CLIENT_UNAVAILABLE) => CLIENT_UNAVAILABLE,
        Some(MISSING) | None => MISSING,
        Some(_) => "heat_status_unrecognized",
    }
}

pub(super) fn score_column(field: &str) -> Option<&'static str> {
    match field {
        "lustre.heat.readsample" => Some("read_samples"),
        "lustre.heat.readbyte" => Some("read_bytes"),
        _ => None,
    }
}

/// Deployment configuration. Policy documents only choose heat fields and
/// thresholds; their authors cannot assign a sampling mount.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadHeatConfig {
    clients: BTreeMap<String, ClientConfig>,
    decay: DecayConfig,
    max_age_seconds: u32,
    pub interval_seconds: u32,
    batch_size: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum ClientConfig {
    Mount(PathBuf),
    Agent(AgentConfig),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase"), deny_unknown_fields)]
struct DecayConfig {
    period_seconds: u32,
    percentage: u8,
}

#[derive(Clone)]
pub(crate) struct HeatProfile {
    filesystem: FileSystemConfig,
    config: ReadHeatConfig,
    executor_mounts: Arc<[PathBuf]>,
    http: Option<reqwest::Client>,
    sampling: Arc<Mutex<()>>,
    // This canonical JSON is hashed in SQL. Changing a client's mount or the
    // executor path invalidates samples from the earlier deployment.
    identity: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewHeat {
    clients: Vec<String>,
    decay: Option<DecayConfig>,
    max_age_seconds: Option<u32>,
    pub(crate) deferred_by_reason: BTreeMap<String, u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observations: Option<Vec<ClientObservation>>,
}

#[derive(sqlx::FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ClientObservation {
    client_id: String,
    observed_at: Option<DateTime<Utc>>,
    read_samples: Option<u64>,
    read_bytes: Option<u64>,
    period_seconds: Option<u32>,
    decay_percentage: Option<u8>,
    status: String,
}

impl HeatProfile {
    pub(crate) fn new(
        filesystem: FileSystemConfig, config: ReadHeatConfig, executor_mounts: Arc<[PathBuf]>,
    ) -> Result<Self, PolicyError> {
        if filesystem.backend != BackendKind::Lustre
            || !(1..=32).contains(&config.clients.len())
            || !(1..=MAX_BATCH_SIZE).contains(&config.batch_size)
            || config.interval_seconds == 0
            || config.max_age_seconds < config.interval_seconds
            || config.decay.period_seconds == 0
            || config.decay.percentage > 100
        {
            return Err(invalid(
                "invalid Lustre read_heat client count, batch size, interval, max age or decay",
            ));
        }
        let mut paths = std::collections::BTreeSet::new();
        let mut has_remote = false;
        for (client, target) in &config.clients {
            if client.len() > 63 || !valid_name(client) {
                return Err(invalid("read_heat requires valid client names of up to 63 characters"));
            }
            match target {
                ClientConfig::Mount(mount) => {
                    if !mount.is_absolute()
                        || mount == &filesystem.mount_path
                        || executor_mounts.contains(mount)
                        || !paths.insert(mount)
                    {
                        return Err(invalid(format!(
                            "read_heat client {client} requires a distinct absolute business mount path"
                        )));
                    }
                }
                ClientConfig::Agent(agent) => {
                    if !agent.validate() {
                        return Err(invalid(format!(
                            "read_heat client {client} requires an HTTPS endpoint (HTTP only on loopback) and an absolute token_file"
                        )));
                    }
                    has_remote = true;
                }
            }
        }
        let http = has_remote
            .then(agent::client)
            .transpose()
            .map_err(|error| PolicyError::Store(error.to_string()))?;
        let identity = serde_json::to_string(&(&filesystem.mount_path, &config.clients, executor_mounts.as_ref()))?;
        Ok(Self {
            filesystem,
            config,
            executor_mounts,
            http,
            sampling: Arc::default(),
            identity,
        })
    }

    pub(crate) async fn sample_batch(
        &self, pool: &MySqlPool, policy: &CompiledPolicy, mut after: [u8; 16],
    ) -> Result<(usize, [u8; 16]), PolicyError> {
        let mut fids = batch_fids(pool, policy, after, self.config.batch_size).await?;
        if fids.is_empty() && after != [0; 16] {
            after = [0; 16];
            fids = batch_fids(pool, policy, after, self.config.batch_size).await?;
        }
        if fids.is_empty() {
            return Ok((0, [0; 16]));
        }
        let count = fids.len();
        let next = if count < usize::from(self.config.batch_size) {
            [0; 16]
        } else {
            fid_codec::encode(&fids[count - 1])
        };
        let _slot = self.sampling.lock().await;
        self.collect_and_record(pool, fids).await?;
        Ok((count, next))
    }

    fn fresh_after(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now - TimeDelta::seconds(i64::from(self.config.max_age_seconds))
    }

    #[tracing::instrument(skip(self, pool), fields(filesystem = %self.filesystem.id))]
    pub(crate) async fn refresh_for_admission(
        &self, pool: &MySqlPool, fid: LuFid,
    ) -> Result<Option<&'static str>, PolicyError> {
        let Ok(slot) = tokio::time::timeout(Duration::from_secs(1), self.sampling.clone().lock_owned()).await else {
            return Ok(Some("heat_sampling_busy"));
        };
        let profile = self.clone();
        let pool = pool.clone();
        // Local Lustre I/O can stay blocked after a deadline. Keep the
        // sampling slot with the task so another refresh cannot pile up.
        let task = tokio::spawn(async move {
            let _slot = slot;
            let result = profile.collect_and_record(&pool, vec![fid]).await;
            if let Err(error) = &result {
                tracing::warn!(filesystem = %profile.filesystem.id, %fid, %error, "admission heat refresh failed");
            }
            result
        });
        match tokio::time::timeout(Duration::from_secs(6), task).await {
            Ok(result) => {
                result.map_err(|error| PolicyError::Store(error.to_string()))??;
                Ok(None)
            }
            Err(_) => Ok(Some("heat_sampling_timeout")),
        }
    }

    async fn collect_and_record(&self, pool: &MySqlPool, fids: Vec<LuFid>) -> Result<(), PolicyError> {
        let observations = collect_batch(self, fids).await?;
        record_batch(pool, self, observations).await
    }
}

pub(super) const UNKNOWN_SCORES: &str =
    " JOIN (SELECT NULL AS read_samples, NULL AS read_bytes, NULL AS heat_status) AS read_heat ";

async fn batch_fids(
    pool: &MySqlPool, policy: &CompiledPolicy, after: [u8; 16], limit: u16,
) -> Result<Vec<LuFid>, PolicyError> {
    let mut query = QueryBuilder::new("SELECT object_id FROM scoped_entries AS entries");
    query
        .push(UNKNOWN_SCORES)
        .push("WHERE filesystem_id = ")
        .push_bind(policy.filesystem.as_str())
        .push(" AND object_kind = 0 AND object_id > ")
        .push_bind(after.to_vec())
        .push(" AND ");
    policy.push_match(&mut query, None);
    query
        .push(" IS NULL ORDER BY object_id LIMIT ")
        .push_bind(i64::from(limit));
    query
        .build_query_scalar::<Vec<u8>>()
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|bytes| fid_codec::decode(&bytes).ok_or_else(|| PolicyError::Store("invalid sampling FID".into())))
        .collect()
}

struct Observation {
    client: String,
    fid: LuFid,
    at: DateTime<Utc>,
    heat: Option<ReadHeat>,
    decay: Option<Decay>,
    failure: Option<&'static str>,
}

async fn collect_batch(profile: &HeatProfile, fids: Vec<LuFid>) -> Result<Vec<Observation>, PolicyError> {
    let executor_path = profile.filesystem.mount_path.clone();
    let other_executors = profile.executor_mounts.clone();
    let executors = tokio::task::spawn_blocking(move || {
        let first = HeatMount::open(&executor_path)?;
        let mut executors = vec![first];
        for path in other_executors.iter().filter(|path| **path != executor_path) {
            if let Some(peer) = HeatMount::open_peer(path, &executors[0].identity().source)? {
                executors.push(peer);
            }
        }
        Ok::<_, lustre_api::LustreApiError>(executors)
    })
    .await
    .map_err(|error| PolicyError::Store(error.to_string()))?;
    let executors = match executors {
        Ok(executors) => executors,
        Err(error) => {
            tracing::debug!(filesystem = %profile.filesystem.id, %error, "could not verify heat sampling mount isolation");
            return Ok(profile
                .config
                .clients
                .keys()
                .flat_map(|client| missing_batch(client, &fids, HeatFailure::ExecutorMountUnavailable))
                .collect());
        }
    };
    let request = Arc::new(SampleRequest {
        source: executors[0].identity().source.clone(),
        fids,
    });
    let mut samples = tokio::task::JoinSet::new();
    for (client, target) in &profile.config.clients {
        let request = request.clone();
        let http = profile.http.clone();
        let client = client.clone();
        let target = target.clone();
        samples.spawn(async move {
            let response = match target {
                ClientConfig::Mount(mount) => tokio::task::spawn_blocking(move || sample_mount(&mount, &request))
                    .await
                    .unwrap_or(Err(HeatFailure::ClientMountUnavailable)),
                ClientConfig::Agent(agent) => {
                    agent
                        .sample(&http.expect("validated remote client transport"), &request)
                        .await
                }
            };
            (client, response)
        });
    }

    let mut responses = BTreeMap::new();
    let mut join_error = None;
    while let Some(response) = samples.join_next().await {
        match response {
            Ok((client, response)) => {
                responses.insert(client, response);
            }
            Err(error) => {
                join_error.get_or_insert(error);
            }
        }
    }
    if let Some(error) = join_error {
        return Err(PolicyError::Store(error.to_string()));
    }
    let capacity = request.fids.len() * responses.len();
    let mut observations = Vec::with_capacity(capacity);
    for (client, response) in &responses {
        let batch = match verify_source(response, &responses, &executors) {
            Ok(batch) => batch,
            Err(reason) => {
                observations.extend(missing_batch(client, &request.fids, reason));
                continue;
            }
        };
        observations.extend(batch.readings.iter().map(|reading| Observation {
            client: client.clone(),
            fid: reading.fid,
            at: reading.observed_at,
            heat: reading.heat.ok(),
            decay: Some(batch.decay),
            failure: reading.heat.err().map(HeatFailure::code),
        }));
    }
    Ok(observations)
}

fn verify_source<'a>(
    response: &'a SampleResult, all: &BTreeMap<String, SampleResult>, executors: &[HeatMount],
) -> Result<&'a agent::HeatBatch, HeatFailure> {
    let batch = response.as_ref().map_err(|reason| *reason)?;
    if batch.mount.source != executors[0].identity().source {
        return Err(HeatFailure::ClientFilesystemMismatch);
    }
    let duplicates = all
        .values()
        .filter_map(|response| response.as_ref().ok())
        .filter(|other| other.mount == batch.mount)
        .count();
    if executors.iter().any(|mount| mount.identity() == &batch.mount) || duplicates != 1 {
        return Err(HeatFailure::ClientMountNotIsolated);
    }
    Ok(batch)
}

fn missing_batch<'a>(
    client: &'a str, fids: &'a [LuFid], failure: HeatFailure,
) -> impl Iterator<Item = Observation> + 'a {
    let at = Utc::now();
    fids.iter().map(move |&fid| Observation {
        client: client.to_owned(),
        fid,
        at,
        heat: None,
        decay: None,
        failure: Some(failure.code()),
    })
}

async fn record_batch(
    pool: &MySqlPool, profile: &HeatProfile, observations: Vec<Observation>,
) -> Result<(), PolicyError> {
    let mut tx = pool.begin().await?;
    for batch in observations.chunks(128) {
        let mut query = QueryBuilder::new(
            "INSERT INTO lustre_read_heat
             (filesystem_id, object_kind, object_id, client_id, profile_hash, observed_at,
              read_samples, read_bytes, period_seconds, decay_percentage, failure)
             SELECT e.filesystem_id, e.object_kind, e.object_id, batch.client, UNHEX(SHA2(",
        );
        query.push_bind(&profile.identity).push(
            ", 256)), batch.sample_time, batch.samples, batch.bytes, batch.period, batch.decay, batch.error
             FROM (",
        );
        let mut rows = query.separated(" UNION ALL ");
        for sample in batch {
            rows.push("SELECT CAST(")
                .push_bind_unseparated(fid_codec::encode(&sample.fid).to_vec())
                .push_unseparated(" AS BINARY(16)) AS fid, ")
                .push_bind_unseparated(&sample.client)
                .push_unseparated(" AS client, ")
                .push_bind_unseparated(sample.at)
                .push_unseparated(" AS sample_time, ")
                .push_bind_unseparated(sample.heat.map(|heat| heat.samples))
                .push_unseparated(" AS samples, ")
                .push_bind_unseparated(sample.heat.map(|heat| heat.bytes))
                .push_unseparated(" AS bytes, ")
                .push_bind_unseparated(sample.decay.map(|decay| decay.period_seconds))
                .push_unseparated(" AS period, ")
                .push_bind_unseparated(sample.decay.map(|decay| decay.percentage))
                .push_unseparated(" AS decay, ")
                .push_bind_unseparated(sample.failure)
                .push_unseparated(" AS error");
        }
        query.push(") AS batch JOIN scoped_entries AS e ON e.object_id = batch.fid
                    WHERE e.object_kind = 0 AND e.filesystem_id = ")
            .push_bind(profile.filesystem.id.as_str())
            .push(
                " ON DUPLICATE KEY UPDATE
                   profile_hash = IF(VALUES(observed_at) >= observed_at, VALUES(profile_hash), profile_hash),
                   read_samples = IF(VALUES(observed_at) >= observed_at, VALUES(read_samples), read_samples),
                   read_bytes = IF(VALUES(observed_at) >= observed_at, VALUES(read_bytes), read_bytes),
                   period_seconds = IF(VALUES(observed_at) >= observed_at, VALUES(period_seconds), period_seconds),
                   decay_percentage = IF(VALUES(observed_at) >= observed_at, VALUES(decay_percentage), decay_percentage),
                   failure = IF(VALUES(observed_at) >= observed_at, VALUES(failure), failure),
                   observed_at = GREATEST(observed_at, VALUES(observed_at))",
            );
        query.build().persistent(false).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct PreviewTotals {
    deferred_reason: Option<String>,
    row_count: u64,
    candidate_count: u64,
    bytes: u64,
}

pub(super) fn missing_reason(profile: Option<&HeatProfile>) -> &'static str {
    if profile.is_some() { MISSING } else { NOT_CONFIGURED }
}

pub(super) async fn preview<Revision>(
    entries: &EntryStore, policy: &CompiledPolicy, profile: Option<&HeatProfile>, revision: Revision,
    resource_bindings: Vec<ResourceBinding>, fid: Option<LuFid>,
) -> Result<PreviewResult<Revision>, PolicyError> {
    // Establish the read view before taking the freshness reference, so the
    // preview cannot see samples committed after the reference time.
    let mut tx = entries
        .pool()
        .begin_with("START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY")
        .await?;
    let now = Utc::now();
    let missing = missing_reason(profile);
    // Compute candidate sums and deferred counts from the same snapshot.
    let mut query = QueryBuilder::new(format!(
        "SELECT CASE WHEN matched IS NULL THEN COALESCE(heat_status, '{missing}') END AS deferred_reason,
         CAST(COUNT(*) AS UNSIGNED) AS row_count,
         CAST(SUM(matched IS TRUE) AS UNSIGNED) AS candidate_count,
         CAST(SUM(IF(matched IS TRUE, size, 0)) AS UNSIGNED) AS bytes
         FROM (SELECT size, heat_status, "
    ));
    policy.push_match(&mut query, None);
    query.push(" AS matched FROM scoped_entries AS entries ");
    if let Some(profile) = profile {
        push_scores(&mut query, profile, now, None);
    } else {
        query.push(UNKNOWN_SCORES);
    }
    query
        .push(" WHERE entries.filesystem_id = ")
        .push_bind(policy.filesystem.as_str())
        .push(" AND entries.object_kind = 0) AS evaluated GROUP BY deferred_reason");
    let rows = query.build_query_as::<PreviewTotals>().fetch_all(&mut *tx).await?;
    let observations = match (profile, fid) {
        (Some(profile), Some(fid)) => Some(observations(&mut tx, profile, now, fid).await?),
        _ => None,
    };
    tx.commit().await?;
    let heat = heat_summary(
        profile,
        rows.iter()
            .filter_map(|row| Some((row.deferred_reason.clone()?, row.row_count)))
            .collect(),
        observations,
    );
    Ok(PreviewResult {
        revision,
        candidate_count: rows.iter().map(|row| row.candidate_count).sum(),
        estimated_bytes: rows.iter().map(|row| row.bytes).sum(),
        estimated_bytes_exact: None,
        resource_bindings,
        read_heat: Some(heat),
        directory_scope: None,
    })
}

pub(super) fn heat_summary(
    profile: Option<&HeatProfile>, deferred_by_reason: BTreeMap<String, u64>,
    observations: Option<Vec<ClientObservation>>,
) -> PreviewHeat {
    PreviewHeat {
        clients: profile
            .map(|profile| profile.config.clients.keys().cloned().collect())
            .unwrap_or_default(),
        decay: profile.map(|profile| profile.config.decay),
        max_age_seconds: profile.map(|profile| profile.config.max_age_seconds),
        deferred_by_reason,
        observations,
    }
}

pub(crate) fn push_scores(
    query: &mut QueryBuilder<MySql>, profile: &HeatProfile, now: DateTime<Utc>, fid: Option<&[u8]>,
) {
    query
        .push(
            " LEFT JOIN (
             SELECT object_id,
                    IF(heat_status IS NULL, read_samples, NULL) AS read_samples,
                    IF(heat_status IS NULL, read_bytes, NULL) AS read_bytes,
                    heat_status
             FROM (
                 SELECT object_id, MAX(read_samples) AS read_samples, MAX(read_bytes) AS read_bytes,
                        CASE WHEN SUM(failure IN (",
        )
        .push_bind(HeatFailure::ClientAgentUnavailable.code())
        .push(", ")
        .push_bind(HeatFailure::ClientMountUnavailable.code())
        .push(")) > 0 THEN ")
        .push_bind(CLIENT_UNAVAILABLE)
        .push(
            " WHEN COUNT(IF(failure IS NULL AND read_samples IS NOT NULL
                                           AND read_bytes IS NOT NULL AND period_seconds IS NOT NULL
                                           AND decay_percentage IS NOT NULL, 1, NULL)) <> ",
        )
        .push_bind(profile.config.clients.len() as i64)
        .push(format!(" THEN '{MISSING}' WHEN MIN(observed_at) < "))
        .push_bind(profile.fresh_after(now))
        .push(" OR MAX(observed_at) > ")
        .push_bind(now)
        .push(format!(" THEN '{STALE}' WHEN MIN(period_seconds) <> "))
        .push_bind(profile.config.decay.period_seconds)
        .push(" OR MAX(period_seconds) <> ")
        .push_bind(profile.config.decay.period_seconds)
        .push(" OR MIN(decay_percentage) <> ")
        .push_bind(profile.config.decay.percentage)
        .push(" OR MAX(decay_percentage) <> ")
        .push_bind(profile.config.decay.percentage)
        .push(format!(
            " THEN '{MISMATCH}' END AS heat_status
                FROM lustre_read_heat WHERE filesystem_id = "
        ))
        .push_bind(profile.filesystem.id.as_str().to_owned())
        .push(" AND object_kind = 0 AND profile_hash = UNHEX(SHA2(")
        .push_bind(profile.identity.clone())
        .push(", 256)) AND client_id IN (");
    {
        let mut clients = query.separated(", ");
        for client in profile.config.clients.keys() {
            clients.push_bind(client.clone());
        }
    }
    query.push(")");
    if let Some(fid) = fid {
        query.push(" AND object_id = ").push_bind(fid.to_vec());
    }
    query.push(
        " GROUP BY object_id
         ) AS summary
         ) AS read_heat ON read_heat.object_id = entries.object_id ",
    );
}

pub(crate) async fn dispatch_deferrals(
    tx: &mut sqlx::Transaction<'_, MySql>, policy: &CompiledPolicy, profile: &HeatProfile, now: DateTime<Utc>,
    database_time_micros: Option<i64>, after: &[u8], through: Option<&[u8]>,
) -> Result<BTreeMap<String, u64>, PolicyError> {
    let mut query = QueryBuilder::new("SELECT COALESCE(heat_status, ");
    query
        .push_bind(MISSING)
        .push(") AS reason, CAST(COUNT(*) AS UNSIGNED) FROM scoped_entries AS entries ");
    push_scores(&mut query, profile, now, None);
    query
        .push(" WHERE entries.filesystem_id = ")
        .push_bind(policy.filesystem.as_str())
        .push(" AND entries.object_kind = 0 AND entries.object_id > ")
        .push_bind(after.to_vec());
    if let Some(through) = through {
        query.push(" AND entries.object_id <= ").push_bind(through.to_vec());
    }
    query.push(" AND ");
    policy.push_match(&mut query, database_time_micros);
    query.push(" IS NULL GROUP BY reason");
    Ok(query.build_query_as().fetch_all(&mut **tx).await?.into_iter().collect())
}

pub(super) async fn observations(
    tx: &mut sqlx::Transaction<'_, MySql>, profile: &HeatProfile, now: DateTime<Utc>, fid: LuFid,
) -> Result<Vec<ClientObservation>, PolicyError> {
    let mut rows: BTreeMap<String, ClientObservation> = sqlx::query_as::<_, ClientObservation>(
        "SELECT CONVERT(client_id USING utf8mb4) AS client_id, observed_at, read_samples, read_bytes, period_seconds, decay_percentage,
         CASE WHEN failure IS NOT NULL THEN failure
              WHEN observed_at < ? OR observed_at > ? THEN 'heat_sample_stale'
              WHEN period_seconds <> ? OR decay_percentage <> ? THEN 'heat_decay_mismatch'
              ELSE 'observed' END AS status
         FROM lustre_read_heat WHERE filesystem_id = ? AND object_kind = 0 AND object_id = ?
         AND profile_hash = UNHEX(SHA2(?, 256))",
    )
    .bind(profile.fresh_after(now))
    .bind(now)
    .bind(profile.config.decay.period_seconds)
    .bind(profile.config.decay.percentage)
    .bind(profile.filesystem.id.as_str())
    .bind(fid_codec::encode(&fid).to_vec())
    .bind(&profile.identity)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|row| (row.client_id.clone(), row))
    .collect();
    Ok(profile
        .config
        .clients
        .keys()
        .map(|client| {
            rows.remove(client).unwrap_or_else(|| ClientObservation {
                client_id: client.clone(),
                observed_at: None,
                read_samples: None,
                read_bytes: None,
                period_seconds: None,
                decay_percentage: None,
                status: MISSING.into(),
            })
        })
        .collect())
}
