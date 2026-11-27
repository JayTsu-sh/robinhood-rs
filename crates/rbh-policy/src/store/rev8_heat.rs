use std::collections::BTreeMap;

use rbh_entry_store::FileSystemId;

use super::PolicyStore;
use crate::PolicyError;

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
    /// Sample one bounded, keyset-paginated batch on this filesystem. The
    /// daemon keeps a separate sampler per configured filesystem.
    #[tracing::instrument(skip(self, sampler))]
    pub async fn sample_read_heat_batch(
        &self, filesystem: &FileSystemId, sampler: &mut HeatSampler,
    ) -> Result<usize, PolicyError> {
        let Some(profile) = self.read_heat.get(filesystem) else {
            return Ok(0);
        };
        let active: Vec<u64> = sqlx::query_scalar(
            "SELECT id FROM policies WHERE rev8_heat_filesystem = ? AND enabled AND name IS NOT NULL ORDER BY id",
        )
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
        let policy = self.compile_revision(&row.definition, &bindings).await?;
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
