//! The policy's namespace rule. Both indexed and live evidence use paths
//! relative to the configured Lustre root.

#[cfg(test)]
mod lustre_tests;

use std::{collections::BTreeMap, fs::File, os::unix::ffi::OsStrExt, path::Path, sync::Arc, time::Instant};

use lustre_api::{LuFid, LustreApi, migration::MigrationError, path::LiveLinks};
use serde::{Deserialize, Serialize};

use super::invalid;
use crate::PolicyError;

const FIELD: &str = "spec.target.directoryScope";
pub(super) const MAX_PATH_BYTES: usize = 4095;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ScopeDocument {
    root: String,
    #[serde(default)]
    excluded_subtrees: Vec<String>,
}

type Components = Vec<Vec<u8>>;

pub(crate) struct DirectoryScope {
    root: Components,
    excluded_subtrees: Vec<Components>,
}

pub(crate) struct AdmittedLinks {
    scope: Arc<DirectoryScope>,
    selected: LiveLinks,
}

impl AdmittedLinks {
    pub(crate) fn recheck(
        &self, mount: &Path, fid: &LuFid, source: &File, deadline: Instant,
    ) -> Result<(), MigrationError> {
        let observed = LustreApi.migration_link_paths_for_file(mount, fid, source, deadline)?;
        self.scope.check_live(&observed)?;
        if observed != self.selected {
            return Err(MigrationError::Deferred("link_evidence_changed"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(super) enum LinkLocation {
    Allowed,
    ExcludedSubtree,
    OutsideScope,
}

#[derive(Default)]
pub(super) struct LinkEvidence {
    indexed: u64,
    resolved: u64,
    excluded: bool,
    outside: bool,
}

pub(super) enum ScopeDecision {
    Allowed,
    Excluded(ExclusionReason),
    Deferred(DeferralReason),
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionReason {
    LinkInExcludedSubtree,
    LinkOutsideScope,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferralReason {
    IncompleteLinkEvidence,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeSummary {
    pub excluded_by_reason: BTreeMap<ExclusionReason, u64>,
    pub deferred_by_reason: BTreeMap<DeferralReason, u64>,
}

impl TryFrom<ScopeDocument> for DirectoryScope {
    type Error = PolicyError;

    fn try_from(document: ScopeDocument) -> Result<Self, Self::Error> {
        let root = parse_path(&document.root, &format!("{FIELD}.root"))?;
        let mut excluded_subtrees = Vec::new();
        for (index, path) in document.excluded_subtrees.iter().enumerate() {
            let field = format!("{FIELD}.excludedSubtrees[{index}]");
            let path = parse_path(path, &field)?;
            if !path.starts_with(&root) || path.len() == root.len() {
                return Err(invalid(format!("{field} must be a child directory under {FIELD}.root")));
            }
            excluded_subtrees.push(path);
        }
        Ok(Self {
            root,
            excluded_subtrees,
        })
    }
}

impl DirectoryScope {
    pub(crate) fn admit(
        self: Arc<Self>, mount: &Path, fid: &LuFid, deadline: Instant,
    ) -> Result<AdmittedLinks, MigrationError> {
        let selected = LustreApi.migration_link_paths(mount, fid, deadline)?;
        self.check_live(&selected)?;
        Ok(AdmittedLinks { scope: self, selected })
    }

    pub(super) fn locate_parent(&self, components: &[&[u8]]) -> LinkLocation {
        if self.excluded_subtrees.iter().any(|path| starts_with(components, path)) {
            LinkLocation::ExcludedSubtree
        } else if starts_with(components, &self.root) {
            LinkLocation::Allowed
        } else {
            LinkLocation::OutsideScope
        }
    }

    fn check_live(&self, links: &LiveLinks) -> Result<(), MigrationError> {
        let paths = links.relative_paths();
        let expected = u32::try_from(paths.len()).map_err(|_| MigrationError::Deferred("incomplete_link_evidence"))?;
        let mut evidence = LinkEvidence::default();
        for path in paths {
            let location = match path {
                Some(path) => path.parent().map(|parent| self.locate_live_parent(parent)),
                None => Some(LinkLocation::OutsideScope),
            };
            evidence.observe(location);
        }
        let reason = match evidence.decide(expected) {
            ScopeDecision::Allowed => return Ok(()),
            ScopeDecision::Excluded(ExclusionReason::LinkInExcludedSubtree) => "link_in_excluded_subtree",
            ScopeDecision::Excluded(ExclusionReason::LinkOutsideScope) => "link_outside_scope",
            ScopeDecision::Deferred(DeferralReason::IncompleteLinkEvidence) => "incomplete_link_evidence",
        };
        Err(MigrationError::Deferred(reason))
    }

    fn locate_live_parent(&self, parent: &Path) -> LinkLocation {
        let bytes = parent.as_os_str().as_bytes();
        let components = bytes
            .split(|&byte| byte == b'/')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        self.locate_parent(&components)
    }
}

impl LinkEvidence {
    pub(super) fn observe(&mut self, location: Option<LinkLocation>) {
        self.indexed += 1;
        if let Some(location) = location {
            self.resolved += 1;
            match location {
                LinkLocation::Allowed => {}
                LinkLocation::ExcludedSubtree => self.excluded = true,
                LinkLocation::OutsideScope => self.outside = true,
            }
        }
    }

    pub(super) fn decide(&self, expected_links: u32) -> ScopeDecision {
        let expected_links = u64::from(expected_links);
        if expected_links == 0 || self.indexed > expected_links {
            ScopeDecision::Deferred(DeferralReason::IncompleteLinkEvidence)
        } else if self.excluded {
            ScopeDecision::Excluded(ExclusionReason::LinkInExcludedSubtree)
        } else if self.outside {
            ScopeDecision::Excluded(ExclusionReason::LinkOutsideScope)
        } else if self.resolved != expected_links {
            ScopeDecision::Deferred(DeferralReason::IncompleteLinkEvidence)
        } else {
            ScopeDecision::Allowed
        }
    }
}

pub(super) fn valid_component(name: &[u8]) -> bool {
    (1..=255).contains(&name.len()) && !matches!(name, b"." | b"..") && !name.contains(&b'/') && !name.contains(&0)
}

fn parse_path(path: &str, field: &str) -> Result<Components, PolicyError> {
    if !path.starts_with('/') || path.len() > MAX_PATH_BYTES {
        return Err(invalid(format!(
            "{field} must be a path starting at the Lustre root (/)"
        )));
    }
    path.as_bytes()
        .split(|byte| *byte == b'/')
        .filter(|part| !part.is_empty())
        .map(|part| {
            if valid_component(part) {
                Ok(part.to_vec())
            } else {
                Err(invalid(format!("{field} contains an invalid path component")))
            }
        })
        .collect()
}

fn starts_with(components: &[&[u8]], prefix: &Components) -> bool {
    components.len() >= prefix.len() && components.iter().zip(prefix).all(|(part, expected)| *part == expected)
}
