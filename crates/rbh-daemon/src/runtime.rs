use std::path::PathBuf;

use rbh_entry_store::{BackendCapabilities, BackendKind, FileSystemConfig, FileSystemId, FileSystemIdError};

const DEFAULT_LUSTRE_ID: &str = "lustre";
const DEFAULT_LUSTRE_MOUNT: &str = "/lustre";

#[derive(Debug, Clone)]
pub struct FileSystemRuntime {
    pub config: FileSystemConfig,
}

impl FileSystemRuntime {
    pub fn should_start_hsm_poller(&self, poll_secs: u64) -> bool {
        poll_secs > 0 && self.config.capabilities.hsm
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeRegistry {
    runtimes: Vec<FileSystemRuntime>,
    lustre_index: usize,
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
            Some(json) => {
                serde_json::from_str::<Vec<FileSystemConfig>>(json).map_err(RuntimeConfigError::InvalidRegistryJson)?
            }
            None => vec![legacy_lustre_config(legacy_mount, legacy_id)?],
        };

        let runtimes: Vec<_> = configs.into_iter().map(|config| FileSystemRuntime { config }).collect();
        let lustre: Vec<_> = runtimes
            .iter()
            .enumerate()
            .filter(|(_, runtime)| runtime.config.backend == BackendKind::Lustre)
            .map(|(index, _)| index)
            .collect();
        if lustre.len() != 1 {
            return Err(RuntimeConfigError::ExpectedOneLustre { found: lustre.len() });
        }

        Ok(Self {
            runtimes,
            lustre_index: lustre[0],
        })
    }

    pub fn lustre(&self) -> &FileSystemRuntime {
        &self.runtimes[self.lustre_index]
    }

    pub fn iter(&self) -> impl Iterator<Item = &FileSystemRuntime> {
        self.runtimes.iter()
    }
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
    #[error("runtime registry must contain exactly one Lustre filesystem; found {found}")]
    ExpectedOneLustre { found: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_mount_translates_to_a_capable_lustre_runtime() {
        let registry = RuntimeRegistry::resolve(None, Some("/mnt/legacy"), Some("archive-fs")).unwrap();
        let runtime = registry.lustre();

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

        assert_eq!(registry.lustre().config.id.as_str(), "lustre-no-hsm");
        assert!(!registry.lustre().config.capabilities.hsm);
        assert!(!registry.lustre().should_start_hsm_poller(30));
    }

    #[test]
    fn registry_rejects_ambiguous_lustre_selection() {
        let config = legacy_lustre_config(None, None).unwrap();
        let json = serde_json::to_string(&vec![config.clone(), config]).unwrap();

        assert!(matches!(
            RuntimeRegistry::resolve(Some(&json), None, None),
            Err(RuntimeConfigError::ExpectedOneLustre { found: 2 })
        ));
    }

    #[test]
    fn hsm_poller_requires_both_interval_and_capability() {
        let registry = RuntimeRegistry::resolve(None, None, None).unwrap();

        assert!(!registry.lustre().should_start_hsm_poller(0));
        assert!(registry.lustre().should_start_hsm_poller(30));
    }
}
