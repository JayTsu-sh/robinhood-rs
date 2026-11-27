//! Rev.8 native Lustre HSM admission and submission.

mod governance;

use std::{ops::ControlFlow, os::unix::fs::MetadataExt, path::Path, sync::Arc};

use async_trait::async_trait;
use lustre_api::{HsmAction, HsmRequestBuilder, HsmState, HsmUserStateInfo, LuFid, LustreApi};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_source_id: Option<String>,
    pub size: u64,
    pub mtime: i64,
    pub mtime_nanos: i64,
    pub ctime: i64,
    pub ctime_nanos: i64,
}

impl Generation {
    pub(crate) fn same_content(&self, current: &Self) -> bool {
        self.filesystem_name == current.filesystem_name
            && self
                .mount_source_id
                .as_ref()
                .is_none_or(|source| current.mount_source_id.as_ref() == Some(source))
            && self.size == current.size
            && self.mtime == current.mtime
            && self.mtime_nanos == current.mtime_nanos
    }
}

pub enum FidObservation {
    Present(Observation),
    Missing { mount_source_id: String },
}

#[derive(Clone, Debug)]
pub struct Observation {
    pub generation: Generation,
    pub state: HsmUserStateInfo,
}

#[derive(Debug)]
pub enum Submission {
    Accepted,
    Deferred(SubmissionDeferral),
    Rejected {
        error: String,
        class: Option<TerminalError>,
    },
    Unknown(String),
}

/// Admission can expire while the native backend prepares a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionDeferral {
    WindowClosed,
    PolicyRunDuration,
    SystemSafetyRunDuration,
    MountChanged,
    MountIdentityUnavailable,
}

impl SubmissionDeferral {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::WindowClosed => "admission_window_closed",
            Self::PolicyRunDuration => "policy_run_duration",
            Self::SystemSafetyRunDuration => "system_safety_run_duration",
            Self::MountChanged => "hsm_mount_changed",
            Self::MountIdentityUnavailable => "hsm_mount_identity_unavailable",
        }
    }
}

/// Recheck the current time against the admitted run immediately before submission.
pub struct SubmissionGate(pub(crate) Box<dyn Fn() -> ControlFlow<SubmissionDeferral> + Send>);

impl SubmissionGate {
    pub fn check(&self) -> ControlFlow<SubmissionDeferral> {
        (self.0)()
    }
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
    async fn observe(&self, mount: &Path, fid: LuFid) -> Result<FidObservation, String>;
    /// Apply the gate after blocking preparation and before issuing the request.
    async fn submit(
        &self, mount: &Path, fid: LuFid, invocation: Invocation, expected: &Generation, gate: SubmissionGate,
    ) -> Submission;
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

    async fn observe(&self, mount: &Path, fid: LuFid) -> Result<FidObservation, String> {
        let mount = mount.to_owned();
        tokio::task::spawn_blocking(move || {
            let lookup = LustreApi
                .hsm_lookup_fid(&mount, fid)
                .map_err(|error| format!("{error:?}"))?;
            let mount_source_id = source_id(&lookup.mount_source);
            let Some((metadata, state)) = lookup.file else {
                return Ok(FidObservation::Missing { mount_source_id });
            };
            Ok(FidObservation::Present(Observation {
                generation: Generation {
                    filesystem_name: lookup.filesystem_name,
                    mount_source_id: Some(mount_source_id),
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

    async fn submit(
        &self, mount: &Path, fid: LuFid, invocation: Invocation, expected: &Generation, gate: SubmissionGate,
    ) -> Submission {
        use lustre_api::error::{
            HsmOp,
            LustreApiError::{Ffi, Hsm, Io},
        };

        let mount = mount.to_owned();
        let expected_source_id = expected.mount_source_id.clone();
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
                ActionKind::PoolMigrate => {
                    return Submission::Rejected {
                        error: "pool migration requires the layout executor".into(),
                        class: None,
                    };
                }
            };
            let archive_id = if action == HsmAction::Archive {
                invocation.archive_id
            } else {
                0
            };
            let request = HsmRequestBuilder::new(action).archive_id(archive_id).add_fid(fid);
            match request.submit_guarded(&mount, |root| match LustreApi.hsm_mount_source(root) {
                Ok(Some(source)) if expected_source_id.as_ref() == Some(&source_id(&source)) => gate.check(),
                Ok(_) => ControlFlow::Break(SubmissionDeferral::MountChanged),
                Err(error) => {
                    tracing::debug!(%fid, ?error, "HSM submission mount identity unavailable");
                    ControlFlow::Break(SubmissionDeferral::MountIdentityUnavailable)
                }
            }) {
                Ok(ControlFlow::Continue(())) => Submission::Accepted,
                Ok(ControlFlow::Break(reason)) => Submission::Deferred(reason),
                Err(
                    error @ Hsm {
                        op: HsmOp::OpenRoot,
                        errno,
                        ..
                    },
                ) => {
                    if let ControlFlow::Break(reason) = gate.check() {
                        return Submission::Deferred(reason);
                    }
                    let class = if errno == libc::EIO {
                        TerminalError::TransientIo
                    } else {
                        TerminalError::BackendUnavailable
                    };
                    Submission::Rejected {
                        error: error.to_string(),
                        class: Some(class),
                    }
                }
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

fn source_id(source: &str) -> String {
    format!("{:x}", Sha256::digest(source.as_bytes()))
}

impl Invocation {
    pub(crate) fn precondition(&self, state: &HsmUserStateInfo) -> Result<(), &'static str> {
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
            ActionKind::PoolMigrate => return lustre_api::migration::check_migration_hsm(state),
        };
        if eligible { Ok(()) } else { Err("hsm_state_ineligible") }
    }

    pub(crate) fn completed(&self, state: &HsmUserStateInfo) -> bool {
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
            ActionKind::PoolMigrate => false,
        }
    }
}
