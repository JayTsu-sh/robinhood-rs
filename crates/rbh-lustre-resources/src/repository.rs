use std::{collections::BTreeMap, sync::Arc};

use rbh_entry_store::{FileSystemConfig, FileSystemId};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, MySqlPool, QueryBuilder, types::Json};

use crate::{capacity::CapacityHistory, observation::ResourceSnapshot};

#[derive(Clone)]
pub(crate) struct SnapshotRepository {
    pool: MySqlPool,
    observer_id: Arc<str>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Observation {
    pub(crate) config: FileSystemConfig,
    pub(crate) resource: ResourceSnapshot,
    pub(crate) boot_id: Option<String>,
    pub(crate) history: CapacityHistory,
}

impl SnapshotRepository {
    pub(crate) fn new(pool: MySqlPool, observer_id: String) -> Self {
        Self {
            pool,
            observer_id: observer_id.into(),
        }
    }

    #[tracing::instrument(skip(self))]
    pub(crate) async fn load(&self, id: &FileSystemId) -> Result<Option<(u64, Observation)>, sqlx::Error> {
        Ok(sqlx::query_as::<_, (u64, Json<Observation>)>(
            "SELECT revision, observation FROM filesystem_resource_snapshots WHERE observer_id = ? AND filesystem_id = ?",
        )
        .bind(self.observer_id.as_bytes())
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?
        .map(|(revision, Json(observation))| (revision, observation)))
    }

    #[tracing::instrument(skip(self, ids))]
    pub(crate) async fn list<'a>(
        &self, ids: impl IntoIterator<Item = &'a FileSystemId>,
    ) -> Result<BTreeMap<FileSystemId, Observation>, sqlx::Error> {
        let mut query =
            QueryBuilder::<MySql>::new("SELECT observation FROM filesystem_resource_snapshots WHERE observer_id = ");
        query
            .push_bind(self.observer_id.as_bytes())
            .push(" AND filesystem_id IN (");
        let mut params = query.separated(", ");
        for id in ids {
            params.push_bind(id.as_str());
        }
        params.push_unseparated(")");
        Ok(query
            .build_query_scalar::<Json<Observation>>()
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|Json(row)| (row.config.id.clone(), row))
            .collect())
    }

    #[tracing::instrument(skip(self, observation), fields(filesystem = %observation.config.id))]
    pub(crate) async fn save(
        &self, observation: Observation, observed_at: u64, revision: Option<u64>,
    ) -> Result<bool, sqlx::Error> {
        if let Some(revision) = revision {
            let result = sqlx::query(
                r"UPDATE filesystem_resource_snapshots SET revision = revision + 1, observed_at = ?, observation = ?
                  WHERE observer_id = ? AND filesystem_id = ? AND revision = ?",
            )
            .bind(observed_at)
            .bind(Json(&observation))
            .bind(self.observer_id.as_bytes())
            .bind(observation.config.id.as_str())
            .bind(revision)
            .execute(&self.pool)
            .await?;
            return Ok(result.rows_affected() == 1);
        }
        let inserted = sqlx::query(
            "INSERT INTO filesystem_resource_snapshots (observer_id, filesystem_id, observed_at, observation) VALUES (?, ?, ?, ?)",
        ).bind(self.observer_id.as_bytes()).bind(observation.config.id.as_str()).bind(observed_at)
            .bind(Json(&observation)).execute(&self.pool).await;
        match inserted {
            Ok(_) => Ok(true),
            Err(sqlx::Error::Database(error)) if error.is_unique_violation() => Ok(false),
            Err(error) => Err(error),
        }
    }
}
