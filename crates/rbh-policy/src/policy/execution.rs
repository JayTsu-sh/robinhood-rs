//! native Lustre HSM admission and submission.

mod governance;

use std::{
    collections::BTreeSet,
    io,
    ops::ControlFlow,
    os::unix::fs::MetadataExt,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
    thread,
};

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

    pub(crate) fn same_mount(&self, current: &Self) -> bool {
        self.filesystem_name == current.filesystem_name
            && self.mount_source_id.is_some()
            && self.mount_source_id == current.mount_source_id
    }
}

pub enum FidObservation {
    Present(Observation),
    Missing { mount_source_id: String },
}

#[derive(Clone, Debug)]
pub struct Observation {
    pub generation: Generation,
    pub access_time: lustre_api::migration::FileAccessTime,
    pub state: HsmUserStateInfo,
}

#[derive(Clone, Debug)]
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
    DispatchCancelled,
    TickBudget,
    ActuatorBusy,
    PolicyInactive,
    PermitStoreNotSynchronous,
}

impl SubmissionDeferral {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::WindowClosed => "admission_window_closed",
            Self::PolicyRunDuration => "policy_run_duration",
            Self::SystemSafetyRunDuration => "system_safety_run_duration",
            Self::MountChanged => "hsm_mount_changed",
            Self::MountIdentityUnavailable => "hsm_mount_identity_unavailable",
            Self::DispatchCancelled => "native_hsm_dispatch_cancelled",
            Self::TickBudget => "native_hsm_submission_tick_budget",
            Self::ActuatorBusy => "native_hsm_actuator_busy",
            Self::PolicyInactive => "policy_revision_inactive",
            Self::PermitStoreNotSynchronous => "permit_store_requires_synchronous_commit",
        }
    }
}

/// Checks admission and claims the request at the ioctl boundary.
pub struct SubmissionGate {
    status: Box<dyn Fn() -> ControlFlow<SubmissionDeferral> + Send>,
    state: Arc<AtomicU8>,
    _slot: Option<NativeSubmissionSlot>,
}

const OPEN: u8 = 0;
const CLAIMED: u8 = 1;
const CANCELLED: u8 = 2;

pub(crate) struct SubmissionOwner(Arc<AtomicU8>);

impl SubmissionOwner {
    /// Revoke the request, returning whether it has already reached the ioctl boundary.
    pub(crate) fn revoke_unless_claimed(&self) -> bool {
        self.0
            .compare_exchange(OPEN, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
            == Err(CLAIMED)
    }
}

impl Drop for SubmissionOwner {
    fn drop(&mut self) {
        self.revoke_unless_claimed();
    }
}

impl SubmissionGate {
    pub(crate) fn owned(
        status: impl Fn() -> ControlFlow<SubmissionDeferral> + Send + 'static,
    ) -> (Self, SubmissionOwner) {
        let state = Arc::new(AtomicU8::new(OPEN));
        let owner = SubmissionOwner(Arc::clone(&state));
        (
            Self {
                status: Box::new(status),
                state,
                _slot: None,
            },
            owner,
        )
    }

    pub(crate) fn holding(mut self, slot: NativeSubmissionSlot) -> Self {
        self._slot = Some(slot);
        self
    }

    pub(crate) fn status(&self) -> ControlFlow<SubmissionDeferral> {
        if self.state.load(Ordering::Acquire) != OPEN {
            return ControlFlow::Break(SubmissionDeferral::DispatchCancelled);
        }
        (self.status)()
    }

    /// Claim a single request. A successful claim is the handoff to the
    /// ioctl. Losing the owner before the claim prevents that handoff.
    pub fn check(&self) -> ControlFlow<SubmissionDeferral> {
        self.status()?;
        if self
            .state
            .compare_exchange(OPEN, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return ControlFlow::Break(SubmissionDeferral::DispatchCancelled);
        }
        ControlFlow::Continue(())
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
    /// Read the state for pool migration, which may outlive a preflight wait.
    async fn observe_for_migration(&self, mount: &Path, fid: LuFid) -> Result<FidObservation, String> {
        self.observe_in_background(mount, fid).await
    }
    /// Read FID state for work that may outlive the scheduler tick.
    async fn observe_in_background(&self, mount: &Path, fid: LuFid) -> Result<FidObservation, String> {
        self.observe(mount, fid).await
    }
    /// Apply the gate after blocking preparation and before issuing the request.
    async fn submit(
        &self, mount: &Path, fid: LuFid, invocation: Invocation, expected: &Generation, gate: SubmissionGate,
    ) -> Submission;
}

pub struct LustreHsm;

// Keep a slot occupied until the system call returns, including after a timeout.
static NATIVE_SUBMISSIONS: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
const MAX_NATIVE_SUBMISSIONS: usize = 64;

pub(crate) struct NativeSubmissionSlot(String);

impl NativeSubmissionSlot {
    pub(crate) fn acquire(filesystem: &str) -> Option<Self> {
        let mut active = NATIVE_SUBMISSIONS.lock().ok()?;
        if active.len() >= MAX_NATIVE_SUBMISSIONS || active.contains(filesystem) {
            return None;
        }
        active.insert(filesystem.to_owned());
        Some(Self(filesystem.to_owned()))
    }
}

impl Drop for NativeSubmissionSlot {
    fn drop(&mut self) {
        if let Ok(mut active) = NATIVE_SUBMISSIONS.lock() {
            active.remove(&self.0);
        }
    }
}

pub(crate) async fn lustre_call<T: Send + 'static>(call: impl FnOnce() -> T + Send + 'static) -> io::Result<T> {
    let (result, pending) = tokio::sync::oneshot::channel();
    // A stalled syscall may outlive its async waiter and must not keep the
    // runtime open. Mutation callers retain their durable operation intent.
    thread::Builder::new().name("policy-lustre".into()).spawn(move || {
        if result.send(call()).is_err() {
            tracing::debug!("Lustre call returned after its waiter stopped");
        }
    })?;
    pending.await.map_err(io::Error::other)
}

fn observe_fid(mount: &Path, fid: LuFid) -> Result<FidObservation, String> {
    let lookup = LustreApi
        .hsm_lookup_fid(mount, fid)
        .map_err(|error| format!("{error:?}"))?;
    let mount_source_id = source_id(&lookup.mount_source);
    let Some((metadata, state)) = lookup.file else {
        return Ok(FidObservation::Missing { mount_source_id });
    };
    Ok(FidObservation::Present(Observation {
        access_time: (&metadata).into(),
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
}

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
        tokio::task::spawn_blocking(move || observe_fid(&mount, fid))
            .await
            .map_err(|error| error.to_string())?
    }

    #[tracing::instrument(skip_all, fields(mount = %mount.display(), %fid))]
    async fn observe_in_background(&self, mount: &Path, fid: LuFid) -> Result<FidObservation, String> {
        let mount = mount.to_owned();
        lustre_call(move || observe_fid(&mount, fid))
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
        let result = lustre_call(move || {
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
                    if let ControlFlow::Break(reason) = gate.status() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_the_dispatch_owner_revokes_an_unclaimed_request() {
        let (gate, owner) = SubmissionGate::owned(|| ControlFlow::Continue(()));
        assert_eq!(gate.status(), ControlFlow::Continue(()));
        drop(owner);
        assert_eq!(
            std::thread::spawn(move || gate.check()).join().unwrap(),
            ControlFlow::Break(SubmissionDeferral::DispatchCancelled)
        );
    }
}
