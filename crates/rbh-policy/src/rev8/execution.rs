//! Rev.8 native Lustre HSM admission and submission.

mod governance;

use std::{os::unix::fs::MetadataExt, path::Path, sync::Arc};

use async_trait::async_trait;
use lustre_api::{HsmAction, HsmState, HsmUserStateInfo, LuFid, LustreApi};
use serde::{Deserialize, Serialize};

use super::{ActionKind, Invocation};
pub(crate) use governance::{Admission, Decision, Profile, Window};

/// Identity supplied by the daemon deployment for its own workload.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Principal {
    pub service_account: String,
    #[serde(default)]
    pub roles: Vec<String>,
}

#[derive(Clone)]
pub struct Runtime {
    pub principal: Principal,
    pub hsm: Arc<dyn NativeHsm>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Generation {
    pub filesystem_name: String,
    pub size: u64,
    pub mtime: i64,
    pub mtime_nanos: i64,
    pub ctime: i64,
    pub ctime_nanos: i64,
}

impl Generation {
    pub(crate) fn same_content(&self, current: &Self) -> bool {
        self.filesystem_name == current.filesystem_name
            && self.size == current.size
            && self.mtime == current.mtime
            && self.mtime_nanos == current.mtime_nanos
    }
}

#[derive(Clone, Debug)]
pub struct Observation {
    pub generation: Generation,
    pub state: HsmUserStateInfo,
}

#[derive(Debug)]
pub enum Submission {
    Accepted,
    Rejected {
        error: String,
        class: Option<TerminalError>,
    },
    Unknown(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalError {
    BackendUnavailable,
    TransientIo,
}

impl TerminalError {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::BackendUnavailable => "backend_unavailable",
            Self::TransientIo => "transient_io",
        }
    }

    pub(crate) fn resolve(name: &str) -> Option<Self> {
        [Self::BackendUnavailable, Self::TransientIo]
            .into_iter()
            .find(|class| class.name() == name)
    }
}

/// Backend boundary for authoritative FID observations and native requests.
#[async_trait]
pub trait NativeHsm: Send + Sync {
    async fn filesystem_name(&self, mount: &Path) -> Result<String, String>;
    async fn observe(&self, mount: &Path, fid: LuFid) -> Result<Option<Observation>, String>;
    async fn submit(&self, mount: &Path, fid: LuFid, invocation: Invocation) -> Submission;
}

pub struct LustreHsm;

// A cancelled async worker can leave a blocking ioctl alive. Retain
// serialization in the local actuator until the actual system call returns.
static NATIVE_SUBMISSION: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[async_trait]
impl NativeHsm for LustreHsm {
    async fn filesystem_name(&self, mount: &Path) -> Result<String, String> {
        let mount = mount.to_owned();
        tokio::task::spawn_blocking(move || {
            let name = LustreApi.hsm_filesystem(&mount).map_err(|error| error.to_string())?;
            if !mount.join(".lustre/fid").is_dir() {
                return Err("configured path must expose the Lustre root .lustre/fid directory".into());
            }
            Ok(name)
        })
        .await
        .map_err(|error| error.to_string())?
    }

    async fn observe(&self, mount: &Path, fid: LuFid) -> Result<Option<Observation>, String> {
        let mount = mount.to_owned();
        tokio::task::spawn_blocking(move || {
            let filesystem_name = LustreApi.hsm_filesystem(&mount).map_err(|error| error.to_string())?;
            let path = rbh_actions::hsm_fid_path(&mount, fid);
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() => metadata,
                Ok(_) => return Err("FID is not a regular file".into()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.to_string()),
            };
            let state = LustreApi
                .hsm_state_and_action(&path)
                .map_err(|error| error.to_string())?;
            Ok(Some(Observation {
                generation: Generation {
                    filesystem_name,
                    size: metadata.size(),
                    mtime: metadata.mtime(),
                    mtime_nanos: metadata.mtime_nsec(),
                    ctime: metadata.ctime(),
                    ctime_nanos: metadata.ctime_nsec(),
                },
                state,
            }))
        })
        .await
        .map_err(|error| error.to_string())?
    }

    async fn submit(&self, mount: &Path, fid: LuFid, invocation: Invocation) -> Submission {
        use lustre_api::error::LustreApiError::{Ffi, Hsm, Io};

        let mount = mount.to_owned();
        let result = tokio::task::spawn_blocking(move || {
            let Ok(_guard) = NATIVE_SUBMISSION.try_lock() else {
                return Submission::Rejected {
                    error: "local actuator is unavailable or still submitting an earlier request".into(),
                    class: None,
                };
            };
            if let Err(error) = LustreApi.hsm_filesystem(&mount) {
                let class = match &error {
                    Io(error) if error.raw_os_error() == Some(libc::EIO) => TerminalError::TransientIo,
                    Ffi { errno: libc::EIO, .. } => TerminalError::TransientIo,
                    _ => TerminalError::BackendUnavailable,
                };
                return Submission::Rejected {
                    error: error.to_string(),
                    class: Some(class),
                };
            }
            let action = match invocation.kind {
                ActionKind::Archive => HsmAction::Archive,
                ActionKind::Release => HsmAction::Release,
                ActionKind::Restore => HsmAction::Restore,
            };
            let archive_id = if action == HsmAction::Archive {
                invocation.archive_id
            } else {
                0
            };
            match rbh_actions::submit_hsm(LustreApi, &mount, fid, action, archive_id, None) {
                Ok(()) => Submission::Accepted,
                // The single-FID request failed validation or access checks.
                Err(
                    error @ Hsm {
                        errno: libc::EPERM | libc::EACCES | libc::EINVAL | libc::ENOTTY | libc::EOPNOTSUPP,
                        ..
                    },
                ) => Submission::Rejected {
                    error: error.to_string(),
                    class: None,
                },
                Err(error) => Submission::Unknown(error.to_string()),
            }
        })
        .await;
        result.unwrap_or_else(|error| Submission::Unknown(error.to_string()))
    }
}

impl Invocation {
    pub(crate) fn precondition(self, state: &HsmUserStateInfo) -> Result<(), &'static str> {
        use HsmState as S;
        if state.has_active_action() {
            return Err("hsm_action_active");
        }
        let flags = state.states;
        if HsmState::from_bits(flags.bits()).is_none() {
            return Err("unknown_hsm_state");
        }
        if self.kind == ActionKind::Archive
            && flags.contains(S::EXISTS)
            && self.archive_id != 0
            && state.archive_id != 0
            && self.archive_id != state.archive_id
        {
            return Err("archive_id_conflict");
        }
        let clean_copy = flags.contains(S::EXISTS | S::ARCHIVED) && !flags.intersects(S::DIRTY | S::LOST);
        let eligible = match self.kind {
            ActionKind::Archive => !flags.intersects(S::NOARCHIVE | S::RELEASED) && !clean_copy,
            ActionKind::Release => clean_copy && !flags.intersects(S::RELEASED | S::NORELEASE),
            ActionKind::Restore => clean_copy && flags.contains(S::RELEASED),
        };
        if eligible { Ok(()) } else { Err("hsm_state_ineligible") }
    }

    pub(crate) fn completed(self, state: &HsmUserStateInfo) -> bool {
        use HsmState as S;
        let flags = state.states;
        if state.has_active_action()
            || HsmState::from_bits(flags.bits()).is_none()
            || !flags.contains(S::EXISTS | S::ARCHIVED)
            || flags.intersects(S::DIRTY | S::LOST)
        {
            return false;
        }
        match self.kind {
            ActionKind::Archive => self.archive_id == 0 || self.archive_id == state.archive_id,
            ActionKind::Release => flags.contains(S::RELEASED) && self.archive_id == state.archive_id,
            ActionKind::Restore => !flags.contains(S::RELEASED) && self.archive_id == state.archive_id,
        }
    }
}
