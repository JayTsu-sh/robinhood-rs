use lustre_api::{LuFid, migration::valid_pool_name, pools::PoolMount};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{duration_millis, invalid, size_bytes};
use crate::PolicyError;

/// Resolved Rev.8 parameters. Per-file stripe defaults are fixed at admission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolMigration {
    pub(crate) source_pool: String,
    pub(crate) target_pool: String,
    pub(crate) stripe_count: Option<u16>,
    pub(crate) stripe_size: Option<u32>,
    pub(crate) max_file_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) min_residence_micros: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Parameters {
    source_pool: Option<String>,
    target_pool: Option<String>,
    stripe_count: Option<u16>,
    stripe_size: Option<Value>,
    max_file_bytes: Option<Value>,
    min_residence: Option<String>,
}

pub(super) fn validate_parameter_names(params: &Map<String, Value>, path: &str) -> Result<(), PolicyError> {
    read_parameters(params.clone(), path).map(|_| ())
}

impl PoolMigration {
    pub(super) fn parse(params: Map<String, Value>, path: &str) -> Result<Self, PolicyError> {
        let params = read_parameters(params, path)?;
        let source_pool = params
            .source_pool
            .ok_or_else(|| invalid(format!("{path}.action.params.sourcePool is required")))?;
        let target_pool = params
            .target_pool
            .ok_or_else(|| invalid(format!("{path}.action.params.targetPool is required")))?;
        if source_pool == target_pool {
            return Err(invalid(format!(
                "{path}.action.params requires different sourcePool and targetPool"
            )));
        }
        let bytes = params
            .max_file_bytes
            .as_ref()
            .and_then(size_bytes)
            .filter(|value| (1..=i64::MAX as u64).contains(value))
            .ok_or_else(|| {
                invalid(format!(
                    "{path}.action.params.maxFileBytes must be a positive size within the supported range"
                ))
            })?;
        let stripe_size = params
            .stripe_size
            .as_ref()
            .map(|size| {
                size_bytes(size)
                    .and_then(|v| u32::try_from(v).ok())
                    .filter(|v| *v != 0 && v.is_multiple_of(65536))
                    .ok_or_else(|| {
                        invalid(format!(
                            "{path}.action.params.stripeSize must be a positive multiple of 64 KiB"
                        ))
                    })
            })
            .transpose()?;
        let min_residence_micros = params
            .min_residence
            .as_deref()
            .map(|value| {
                duration_millis(value)
                    .and_then(|ms| ms.checked_mul(1000))
                    .and_then(|micros| i64::try_from(micros).ok())
                    .ok_or_else(|| {
                        invalid(format!(
                            "{path}.action.params.minResidence must be a positive Rev.8 duration"
                        ))
                    })
            })
            .transpose()?;
        Ok(Self {
            source_pool,
            target_pool,
            stripe_count: params.stripe_count,
            stripe_size,
            max_file_bytes: bytes,
            min_residence_micros,
        })
    }

    #[tracing::instrument(skip(self), fields(mount = %mount.display(), source_pool = %self.source_pool, target_pool = %self.target_pool))]
    pub(crate) async fn validate_pools(&self, mount: &std::path::Path) -> Result<(), PolicyError> {
        self.inspect_mount(mount, false).await.map(|_| ())
    }

    pub(super) async fn preview_root(&self, mount: &std::path::Path) -> Result<LuFid, PolicyError> {
        self.inspect_mount(mount, true)
            .await?
            .ok_or_else(|| invalid("spec.target.directoryScope requires a Lustre root FID"))
    }

    async fn inspect_mount(&self, mount: &std::path::Path, require_root: bool) -> Result<Option<LuFid>, PolicyError> {
        let path = mount.to_owned();
        let migration = self.clone();
        tokio::task::spawn_blocking(move || {
            let mount = PoolMount::open(&path)
                .map_err(|error| invalid(format!("lustre.pool_migrate mount {}: {error}", path.display())))?;
            for (field, pool) in [
                ("sourcePool", &migration.source_pool),
                ("targetPool", &migration.target_pool),
            ] {
                let members = mount
                    .pool_members(pool)
                    .map_err(|error| invalid(format!("lustre.pool_migrate {field} {pool}: {error}")))?;
                if members.is_empty() {
                    return Err(invalid(format!("lustre.pool_migrate {field} {pool} has no OSTs")));
                }
                if field == "targetPool"
                    && migration
                        .stripe_count
                        .is_some_and(|count| usize::from(count) > members.len())
                {
                    return Err(invalid(
                        "lustre.pool_migrate stripeCount exceeds target pool membership",
                    ));
                }
            }
            require_root
                .then(|| {
                    mount
                        .directory_fid()
                        .map_err(|error| invalid(format!("spec.target.directoryScope root FID: {error}")))
                })
                .transpose()
        })
        .await
        .map_err(|error| PolicyError::Store(error.to_string()))?
    }
}

fn read_parameters(params: Map<String, Value>, path: &str) -> Result<Parameters, PolicyError> {
    let params: Parameters = serde_path_to_error::deserialize(&Value::Object(params))
        .map_err(|error| invalid(format!("{path}.action.params: {error}")))?;
    for (field, pool) in [("sourcePool", &params.source_pool), ("targetPool", &params.target_pool)] {
        if pool.as_deref().is_some_and(|pool| !valid_pool_name(pool)) {
            return Err(invalid(format!(
                "{path}.action.params.{field} must be an unqualified Lustre pool name"
            )));
        }
    }
    if params.stripe_count == Some(0) {
        return Err(invalid(format!("{path}.action.params.stripeCount must be positive")));
    }
    Ok(params)
}
