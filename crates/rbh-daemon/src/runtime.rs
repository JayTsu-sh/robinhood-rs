use std::path::PathBuf;

use rbh_entry_store::{BackendCapabilities, BackendKind, FileSystemConfig, FileSystemId, FileSystemIdError};
use serde::Deserialize;

const DEFAULT_LUSTRE_ID: &str = "lustre";
const DEFAULT_LUSTRE_MOUNT: &str = "/lustre";

#[derive(Debug, Clone)]
pub struct FileSystemRuntime {
    pub config: FileSystemConfig,
    pub changelog_agent: Option<JuiceFsAgentConfig>,
    pub lustre_changelog: Vec<LustreChangelogConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct JuiceFsAgentConfig {
    pub endpoint: String,
    pub volume: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LustreChangelogConfig {
    pub mdt: String,
    pub reader_id: String,
}

#[derive(Deserialize)]
struct RuntimeInput {
    #[serde(flatten)]
    config: FileSystemConfig,
    #[serde(default)]
    changelog_agent: Option<JuiceFsAgentConfig>,
    #[serde(default)]
    lustre_changelog: Vec<LustreChangelogConfig>,
}

impl FileSystemRuntime {
    pub fn should_start_hsm_poller(&self, poll_secs: u64) -> bool {
        poll_secs > 0 && self.config.capabilities.hsm
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeRegistry {
    runtimes: Vec<FileSystemRuntime>,
}

impl RuntimeRegistry {
    pub fn from_env() -> Result<Self, RuntimeConfigError> {
        let explicit = std::env::var("RBH_FILESYSTEMS_JSON").ok();
        let legacy_mount = std::env::var("RBH_LUSTRE_MOUNT").ok();
        let legacy_id = std::env::var("RBH_FILESYSTEM_ID").ok();
        Self::resolve(explicit.as_deref(), legacy_mount.as_deref(), legacy_id.as_deref())
    }

    fn resolve(
        explicit_json: Option<&str>, legacy_mount: Option<&str>, legacy_id: Option<&str>,
    ) -> Result<Self, RuntimeConfigError> {
        let configs = match explicit_json.filter(|value| !value.trim().is_empty()) {
            Some(json) => serde_json::from_str::<Vec<RuntimeInput>>(json)
                .map_err(RuntimeConfigError::InvalidRegistryJson)?
                .into_iter()
                .map(|input| FileSystemRuntime {
                    config: input.config,
                    changelog_agent: input.changelog_agent,
                    lustre_changelog: input.lustre_changelog,
                })
                .collect(),
            None => vec![FileSystemRuntime {
                config: legacy_lustre_config(legacy_mount, legacy_id)?,
                changelog_agent: None,
                lustre_changelog: legacy_lustre_changelog(),
            }],
        };

        Ok(Self { runtimes: configs })
    }

    pub fn iter(&self) -> impl Iterator<Item = &FileSystemRuntime> {
        self.runtimes.iter()
    }
}

fn legacy_lustre_changelog() -> Vec<LustreChangelogConfig> {
    let mdts = std::env::var("RBH_MDTS").ok();
    let legacy_mdt = std::env::var("RBH_MDT_NAME").ok();
    let users = std::env::var("RBH_CHANGELOG_USER").unwrap_or_default();
    super::pair_mdts_with_users(mdts.as_deref(), legacy_mdt.as_deref(), &users)
        .into_iter()
        .map(|(mdt, reader_id)| LustreChangelogConfig { mdt, reader_id })
        .collect()
}

fn legacy_lustre_config(mount: Option<&str>, id: Option<&str>) -> Result<FileSystemConfig, RuntimeConfigError> {
    Ok(FileSystemConfig {
        id: FileSystemId::new(id.unwrap_or(DEFAULT_LUSTRE_ID))?,
        backend: BackendKind::Lustre,
        mount_path: PathBuf::from(mount.unwrap_or(DEFAULT_LUSTRE_MOUNT)),
        capabilities: BackendCapabilities {
            changelog: true,
            namespace: true,
            purge: true,
            hsm: true,
            stripe: true,
            ost: true,
        },
    })
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeConfigError {
    #[error("invalid filesystem id in legacy configuration: {0}")]
    InvalidFileSystemId(#[from] FileSystemIdError),
    #[error("RBH_FILESYSTEMS_JSON is invalid: {0}")]
    InvalidRegistryJson(serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_mount_translates_to_a_capable_lustre_runtime() {
        let registry = RuntimeRegistry::resolve(None, Some("/mnt/legacy"), Some("archive-fs")).unwrap();
        let runtime = registry.iter().next().unwrap();

        assert_eq!(runtime.config.id.as_str(), "archive-fs");
        assert_eq!(runtime.config.mount_path, PathBuf::from("/mnt/legacy"));
        assert_eq!(runtime.config.backend, BackendKind::Lustre);
        assert!(runtime.config.capabilities.changelog);
        assert!(runtime.config.capabilities.namespace);
        assert!(runtime.config.capabilities.hsm);
    }

    #[test]
    fn explicit_registry_controls_hsm_capability() {
        let json = r#"[{
            "id":"lustre-no-hsm",
            "backend":"lustre",
            "mount_path":"/mnt/lustre",
            "capabilities":{
                "changelog":true,"namespace":true,"purge":true,
                "hsm":false,"stripe":true,"ost":true
            }
        }]"#;
        let registry = RuntimeRegistry::resolve(Some(json), Some("/ignored"), None).unwrap();

        let runtime = registry.iter().next().unwrap();
        assert_eq!(runtime.config.id.as_str(), "lustre-no-hsm");
        assert!(!runtime.config.capabilities.hsm);
        assert!(!runtime.should_start_hsm_poller(30));
    }

    #[test]
    fn explicit_juicefs_runtime_carries_plaintext_agent_configuration() {
        let json = r#"[
          {"id":"lustre","backend":"lustre","mount_path":"/lustre","capabilities":{"changelog":true,"namespace":true,"purge":true,"hsm":true,"stripe":true,"ost":true}},
          {"id":"jfs-nfs","backend":"juice_fs","mount_path":"/jfs","capabilities":{"changelog":true,"namespace":true,"purge":true,"hsm":false,"stripe":false,"ost":false},"changelog_agent":{"endpoint":"http://10.131.9.41:9443","volume":"jfs-nfs"}}
        ]"#;
        let registry = RuntimeRegistry::resolve(Some(json), None, None).unwrap();
        let juicefs = registry
            .iter()
            .find(|runtime| runtime.config.id.as_str() == "jfs-nfs")
            .unwrap();
        assert_eq!(juicefs.changelog_agent.as_ref().unwrap().volume, "jfs-nfs");
        assert_eq!(
            juicefs.changelog_agent.as_ref().unwrap().endpoint,
            "http://10.131.9.41:9443"
        );
        assert!(!juicefs.config.capabilities.hsm);
    }

    #[test]
    fn registry_accepts_multiple_lustre_runtimes_without_global_selection() {
        let first = legacy_lustre_config(Some("/mnt/lustre-a"), Some("lustre-a")).unwrap();
        let second = legacy_lustre_config(Some("/mnt/lustre-b"), Some("lustre-b")).unwrap();
        let json = serde_json::to_string(&vec![first, second]).unwrap();

        let registry = RuntimeRegistry::resolve(Some(&json), None, None).unwrap();
        assert_eq!(registry.iter().count(), 2);
    }

    #[test]
    fn registry_accepts_a_juicefs_only_deployment() {
        let json = r#"[{
          "id":"juice-only","backend":"juice_fs","mount_path":"/jfs",
          "capabilities":{"changelog":true,"namespace":true,"purge":true,"hsm":false,"stripe":false,"ost":false},
          "changelog_agent":{"endpoint":"http://agent:9443","volume":"juice-only"}
        }]"#;

        let registry = RuntimeRegistry::resolve(Some(json), None, None).unwrap();
        assert_eq!(registry.iter().count(), 1);
        assert_eq!(registry.iter().next().unwrap().config.backend, BackendKind::JuiceFs);
    }

    #[test]
    fn hsm_poller_requires_both_interval_and_capability() {
        let registry = RuntimeRegistry::resolve(None, None, None).unwrap();

        let runtime = registry.iter().next().unwrap();
        assert!(!runtime.should_start_hsm_poller(0));
        assert!(runtime.should_start_hsm_poller(30));
    }
}
