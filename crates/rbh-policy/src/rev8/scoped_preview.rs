//! Evaluate catalog links in the same read view as the Rev.8 rule matches.

use std::collections::{BTreeMap, BTreeSet, HashMap, hash_map::Entry};

use chrono::{DateTime, Utc};
use lustre_api::LuFid;
use rbh_entry_store::{EntryKind, EntryStore, FileSystemConfig, FileSystemId, fid_codec};
use sqlx::{MySqlConnection, QueryBuilder};

use super::{
    CompiledPolicy, DB_EPOCH_MICROS,
    directory_scope::{
        DirectoryScope, LinkEvidence, LinkLocation, MAX_PATH_BYTES, ScopeDecision, ScopeSummary, valid_component,
    },
    heat::{self, HeatProfile, PreviewHeat},
};
use crate::PolicyError;

type Fid = [u8; 16];
const CATALOG_BATCH: usize = 512;
const DIRECTORY_CACHE: usize = 65_536;

#[derive(Default)]
pub(super) struct ScopedTotals {
    pub(super) candidate_count: u64,
    pub(super) estimated_bytes: u64,
    pub(super) scope: ScopeSummary,
    heat_deferrals: BTreeMap<String, u64>,
    pub(super) read_heat: Option<PreviewHeat>,
}

#[derive(sqlx::FromRow)]
struct CatalogLink {
    object_id: Vec<u8>,
    parent_kind: Option<u8>,
    parent_id: Option<Vec<u8>>,
    name: Option<Vec<u8>>,
}

impl CatalogLink {
    fn parent(&self) -> Option<Fid> {
        let name = self.name.as_deref()?;
        if self.parent_kind != Some(0) || !valid_component(name) {
            return None;
        }
        self.parent_id.as_deref()?.try_into().ok()
    }
}

enum DirectoryRecord {
    Parent { id: Fid, name: Vec<u8> },
    Unresolved,
    Missing,
}

struct DirectoryIndex {
    root: Fid,
    directories: HashMap<Fid, DirectoryRecord>,
    locations: HashMap<Fid, Option<LinkLocation>>,
}

impl DirectoryIndex {
    fn new(root: Fid) -> Self {
        Self {
            root,
            directories: HashMap::new(),
            locations: HashMap::new(),
        }
    }

    async fn load_ancestors(
        &mut self, db: &mut MySqlConnection, filesystem: &FileSystemId, page: &[CandidateLink],
    ) -> Result<(), PolicyError> {
        if self.directories.len() >= DIRECTORY_CACHE {
            self.directories.clear();
            self.locations.clear();
        }
        let mut pending: BTreeSet<Fid> = page.iter().filter_map(|candidate| candidate.link.parent()).collect();
        pending.insert(self.root);
        pending.retain(|fid| !self.directories.contains_key(fid));
        while !pending.is_empty() {
            let wave: Vec<Fid> = std::mem::take(&mut pending).into_iter().collect();
            for ids in wave.chunks(CATALOG_BATCH) {
                let mut records = load_directories(db, filesystem, ids).await?;
                for &fid in ids {
                    let record = records.remove(&fid).unwrap_or(DirectoryRecord::Missing);
                    if fid != self.root
                        && let DirectoryRecord::Parent { id, .. } = &record
                    {
                        pending.insert(*id);
                    }
                    self.directories.insert(fid, record);
                }
            }
            pending.retain(|fid| !self.directories.contains_key(fid));
        }
        Ok(())
    }

    fn locate(&mut self, directory: Fid, scope: &DirectoryScope) -> Option<LinkLocation> {
        if let Some(location) = self.locations.get(&directory) {
            return *location;
        }
        let location = self.walk_to_root(directory).map(|parts| scope.locate_parent(&parts));
        self.locations.insert(directory, location);
        location
    }

    fn walk_to_root(&self, mut directory: Fid) -> Option<Vec<&[u8]>> {
        let mut parts = Vec::new();
        let mut path_bytes = 0;
        while directory != self.root {
            let DirectoryRecord::Parent { id, name } = self.directories.get(&directory)? else {
                return None;
            };
            path_bytes += name.len() + 1;
            // The path bound also terminates corrupt ancestor cycles.
            if path_bytes > MAX_PATH_BYTES {
                return None;
            }
            parts.push(name.as_slice());
            directory = *id;
        }
        if matches!(self.directories.get(&self.root)?, DirectoryRecord::Missing) {
            return None;
        }
        parts.reverse();
        Some(parts)
    }
}

async fn load_directories(
    db: &mut MySqlConnection, filesystem: &FileSystemId, ids: &[Fid],
) -> Result<HashMap<Fid, DirectoryRecord>, PolicyError> {
    let mut query = QueryBuilder::new(
        "SELECT dirs.object_id, edge.parent_kind, edge.parent_id, edge.name
         FROM scoped_entries AS dirs
         LEFT JOIN scoped_namespace_edges AS edge USING (filesystem_id, object_kind, object_id)
         WHERE dirs.filesystem_id = ",
    );
    query
        .push_bind(filesystem.as_str())
        .push(" AND dirs.object_kind = 0 AND dirs.kind = ")
        .push_bind(EntryKind::Directory as u8)
        .push(" AND dirs.object_id IN (");
    let mut parameters = query.separated(", ");
    for id in ids {
        parameters.push_bind(id.as_slice());
    }
    query.push(")");
    let rows = query
        .build_query_as::<CatalogLink>()
        .persistent(false)
        .fetch_all(db)
        .await?;
    let mut records = HashMap::new();
    for link in rows {
        match records.entry(indexed_fid(&link.object_id)?) {
            Entry::Vacant(slot) => {
                let record = link
                    .parent()
                    .zip(link.name)
                    .map_or(DirectoryRecord::Unresolved, |(id, name)| DirectoryRecord::Parent {
                        id,
                        name,
                    });
                slot.insert(record);
            }
            Entry::Occupied(mut slot) => {
                // A directory with competing parents cannot prove a path.
                slot.insert(DirectoryRecord::Unresolved);
            }
        }
    }
    Ok(records)
}

#[derive(sqlx::FromRow)]
struct CandidateLink {
    #[sqlx(flatten)]
    link: CatalogLink,
    size: u64,
    nlink: u32,
    heat_reason: Option<String>,
}

impl ScopedTotals {
    fn record(&mut self, file: &CandidateLink, evidence: &LinkEvidence) -> Result<(), PolicyError> {
        match evidence.decide(file.nlink) {
            ScopeDecision::Allowed => {
                if let Some(reason) = &file.heat_reason {
                    *self.heat_deferrals.entry(reason.clone()).or_default() += 1;
                } else {
                    self.candidate_count += 1;
                    self.estimated_bytes = self
                        .estimated_bytes
                        .checked_add(file.size)
                        .ok_or_else(|| PolicyError::Store("preview logical byte total overflow".into()))?;
                }
            }
            ScopeDecision::Excluded(reason) => *self.scope.excluded_by_reason.entry(reason).or_default() += 1,
            ScopeDecision::Deferred(reason) => *self.scope.deferred_by_reason.entry(reason).or_default() += 1,
        }
        Ok(())
    }
}

pub(super) async fn evaluate(
    entries: &EntryStore, policy: &CompiledPolicy, config: &FileSystemConfig, scope: &DirectoryScope,
    profile: Option<&HeatProfile>, heat_fid: Option<LuFid>,
) -> Result<ScopedTotals, PolicyError> {
    let migration = policy
        .pool_migration()
        .expect("directory scopes apply to pool migration");
    let root = fid_codec::encode(&migration.preview_root(&config.mount_path).await?);

    // Explicit isolation keeps the evidence consistent even when the database
    // defaults to READ COMMITTED.
    let mut connection = entries.pool().acquire().await?;
    // Cancellation during transaction setup must discard the pending session state.
    connection.close_on_drop();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *connection)
        .await?;
    let mut tx = sqlx::Connection::begin_with(
        &mut *connection,
        "START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY",
    )
    .await?;
    let now = Utc::now();
    let database_now: i64 = sqlx::query_scalar(&format!("SELECT CAST({DB_EPOCH_MICROS} AS SIGNED)"))
        .fetch_one(&mut *tx)
        .await?;
    let mut directories = DirectoryIndex::new(root);
    let mut totals = ScopedTotals::default();
    let mut after = None;
    loop {
        let page = candidate_page(&mut tx, policy, profile, now, database_now, after).await?;
        let Some(last) = page.last() else {
            break;
        };
        after = Some(indexed_fid(&last.link.object_id)?);
        directories.load_ancestors(&mut tx, &config.id, &page).await?;
        for links in page.chunk_by(|left, right| left.link.object_id == right.link.object_id) {
            let mut evidence = LinkEvidence::default();
            for link in links {
                evidence.observe(link.link.parent().and_then(|parent| directories.locate(parent, scope)));
            }
            totals.record(&links[0], &evidence)?;
        }
    }
    if policy.uses_read_heat {
        let observations = match (profile, heat_fid) {
            (Some(profile), Some(fid)) => Some(heat::observations(&mut tx, profile, now, fid).await?),
            _ => None,
        };
        totals.read_heat = Some(heat::heat_summary(
            profile,
            std::mem::take(&mut totals.heat_deferrals),
            observations,
        ));
    }
    tx.commit().await?;
    Ok(totals)
}

async fn candidate_page(
    db: &mut MySqlConnection, policy: &CompiledPolicy, profile: Option<&HeatProfile>, now: DateTime<Utc>,
    database_now: i64, after: Option<Fid>,
) -> Result<Vec<CandidateLink>, PolicyError> {
    // Limit the FIDs before joining their names, so a file's links stay in one page.
    let mut query = QueryBuilder::new(
        "SELECT page.object_id, page.size, page.nlink,
                CASE WHEN page.matched IS NULL THEN page.heat_reason END AS heat_reason,
                edge.parent_kind, edge.parent_id, edge.name
         FROM (SELECT * FROM (
             SELECT entries.filesystem_id, entries.object_kind, entries.object_id, size, nlink, ",
    );
    if policy.uses_read_heat {
        query
            .push("COALESCE(heat_status, ")
            .push_bind(heat::missing_reason(profile))
            .push(") AS heat_reason, ");
    } else {
        query.push("NULL AS heat_reason, ");
    }
    policy.push_match(&mut query, Some(database_now));
    query.push(" AS matched FROM scoped_entries AS entries ");
    if policy.uses_read_heat {
        if let Some(profile) = profile {
            heat::push_scores(&mut query, profile, now, None);
        } else {
            query.push(heat::UNKNOWN_SCORES);
        }
    }
    query
        .push(" WHERE entries.filesystem_id = ")
        .push_bind(policy.filesystem.as_str())
        .push(" AND entries.object_kind = 0");
    if let Some(after) = after {
        query.push(" AND entries.object_id > ").push_bind(after.to_vec());
    }
    query.push(") AS evaluated WHERE matched ");
    query.push(if policy.uses_read_heat {
        "IS NOT FALSE"
    } else {
        "IS TRUE"
    });
    query
        .push(" ORDER BY object_id LIMIT ")
        .push_bind(CATALOG_BATCH as u32)
        .push(
            ") AS page LEFT JOIN scoped_namespace_edges AS edge USING (filesystem_id, object_kind, object_id)
         ORDER BY page.object_id",
        );
    Ok(query
        .build_query_as::<CandidateLink>()
        .persistent(false)
        .fetch_all(db)
        .await?)
}

fn indexed_fid(bytes: &[u8]) -> Result<Fid, PolicyError> {
    bytes
        .try_into()
        .map_err(|_| PolicyError::Store("invalid catalog FID".into()))
}
