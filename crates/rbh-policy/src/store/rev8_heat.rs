use std::collections::BTreeMap;

use rbh_entry_store::FileSystemId;

use super::PolicyStore;
use crate::{PolicyError, rev8};

const ACTIVE_HEAT_POLICY: &str = "revision IS NOT NULL AND enabled AND name IS NOT NULL";

#[derive(Default)]
pub struct HeatSampler {
    last_policy: u64,
    cursors: BTreeMap<u64, (u64, [u8; 16])>,
}

impl HeatSampler {
    fn cursor(&self, policy: u64, revision: u64) -> [u8; 16] {
        self.cursors
            .get(&policy)
            .filter(|(cursor_revision, _)| *cursor_revision == revision)
            .map(|(_, fid)| *fid)
            .unwrap_or_default()
    }
}

impl PolicyStore {
    /// Scope daemon sampling jobs to deployed profiles used by enabled policies.
    #[tracing::instrument(skip(self))]
    pub async fn active_read_heat_filesystems(&self) -> Result<Vec<FileSystemId>, PolicyError> {
        if self.read_heat.is_empty() {
            return Ok(Vec::new());
        }
        let active: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT DISTINCT rev8_heat_filesystem FROM policies
             WHERE rev8_heat_filesystem IS NOT NULL AND {ACTIVE_HEAT_POLICY}",
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(self
            .read_heat
            .keys()
            .filter(|filesystem| active.iter().any(|id| id == filesystem.as_str()))
            .cloned()
            .collect())
    }

    /// Sample one bounded, keyset-paginated batch on this filesystem. The
    /// daemon keeps a separate sampler per configured filesystem.
    #[tracing::instrument(skip(self, sampler))]
    pub async fn sample_read_heat_batch(
        &self, filesystem: &FileSystemId, sampler: &mut HeatSampler,
    ) -> Result<usize, PolicyError> {
        let Some(profile) = self.read_heat.get(filesystem) else {
            return Ok(0);
        };
        let active: Vec<u64> = sqlx::query_scalar(&format!(
            "SELECT id FROM policies WHERE rev8_heat_filesystem = ? AND {ACTIVE_HEAT_POLICY} ORDER BY id",
        ))
        .bind(filesystem.as_str())
        .fetch_all(&self.pool)
        .await?;
        sampler.cursors.retain(|id, _| active.binary_search(id).is_ok());
        let Some(&id) = active.iter().find(|&&id| id > sampler.last_policy).or(active.first()) else {
            return Ok(0);
        };
        sampler.last_policy = id;
        let Some(row) = self.get_rev8(id).await? else {
            return Ok(0);
        };
        let bindings = row.resource_bindings.map(|bindings| bindings.0).unwrap_or_default();
        let policy = self.compile_revision(rev8::parse(&row.definition)?, &bindings).await?;
        if !policy.enabled || !policy.uses_read_heat || &policy.filesystem != filesystem {
            return Ok(0);
        }
        let after = sampler.cursor(id, row.revision);
        let (count, next) = profile.sample_batch(&self.pool, &policy, after).await?;
        if count == 0 {
            sampler.cursors.remove(&id);
            return Ok(0);
        }
        sampler.cursors.insert(id, (row.revision, next));
        Ok(count)
    }
}
