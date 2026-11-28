use std::{
    ops::ControlFlow,
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::Utc;
use rbh_entry_store::FileSystemId;
use sqlx::{MySql, Transaction};

use super::{
    Candidate, Dispatch, PolicyStore, current_revision, decode_fid, elapsed_ms, lock_permit,
    preflight::{PreflightKey, PreflightRequest, PreflightResult, PreflightWait},
    short_reason, verify_worker_permit,
};
use crate::{
    PolicyError, Rev8TickTask,
    rev8::{
        ActionKind, BudgetLimits, Invocation,
        execution::{
            Admission, Decision, FidObservation, NativeHsm, Observation, Submission, SubmissionDeferral,
            SubmissionGate, SubmissionOwner,
        },
        invalid,
        resources::safety_matches,
    },
};

// Give a fresh verification its own bounded wait after the initial reads
// crossed a turn. A verification slower than this cannot authorize an ioctl.
const FINAL_LOOKUP_LIMIT: Duration = Rev8TickTask::INTERVAL.saturating_mul(2);

pub(in crate::store) type NativePreflight = Result<VerifiedNativeCandidate, String>;

pub(in crate::store) struct VerifiedNativeCandidate {
    invocation: Invocation,
    snapshot: Observation,
    checked_at: Instant,
}

impl PolicyStore {
    pub(super) async fn admit_native(
        &self, dispatch: &Dispatch<'_>, candidate: &Candidate, invocation: Invocation,
        worker: &mut Transaction<'_, MySql>,
    ) -> Result<ControlFlow<&'static str>, PolicyError> {
        let fid = decode_fid(&candidate.fid)?;
        let filesystem = &dispatch.policy.filesystem;
        let deadline = NativeDeadline::new(dispatch, invocation.kind)?;
        let tick_deadline = *dispatch
            .preflight_until
            .get_or_init(|| Instant::now() + Rev8TickTask::INTERVAL);
        let key = PreflightKey {
            run: dispatch.run.id,
            revision: dispatch.run.revision,
            fid,
            branch: candidate.branch,
            mount: dispatch.config.mount_path.clone(),
            catalog_version: (candidate.size, candidate.mtime, candidate.ctime),
            catalog_atime: None,
        };
        let backend = &self.runtime.as_ref().expect("dispatch requires a runtime").hsm;
        let tick_started = tick_deadline - Rev8TickTask::INTERVAL;
        let mut run_deadline = deadline.at;
        let mut previous: Option<VerifiedNativeCandidate> = None;
        let verified = loop {
            let request = PreflightRequest {
                key: key.clone(),
                tick_deadline: if previous.is_some() {
                    Instant::now() + FINAL_LOOKUP_LIMIT
                } else {
                    tick_deadline
                },
                run_deadline,
            };
            let backend = Arc::clone(backend);
            let admission = Arc::clone(&dispatch.admission);
            let filesystem = filesystem.clone();
            let key = key.clone();
            let action = invocation.clone();
            let result = self
                .native_preflights
                .read(&self.pool, &dispatch.policy.filesystem, request, async move {
                    if let Some(previous) = previous {
                        verify_native_candidate(backend.as_ref(), &key, previous.invocation, previous.snapshot).await
                    } else {
                        observe_native_candidate(backend, admission, filesystem, key, action, run_deadline).await
                    }
                })
                .await?;
            match result {
                PreflightResult::Observed(Ok(verified), deadline) => {
                    run_deadline = deadline;
                    if verified.checked_at >= tick_started {
                        break verified;
                    }
                    // A result from an earlier worker turn is still useful
                    // for comparison. Verify it again before it can dispatch.
                    previous = Some(verified);
                }
                PreflightResult::Observed(Err(reason), _) => {
                    return self
                        .record_deferred(dispatch, candidate, &invocation, &reason)
                        .await
                        .map(ControlFlow::Continue);
                }
                PreflightResult::Waiting(wait)
                    if matches!(wait, PreflightWait::Pending)
                        || (dispatch.policy.one_shot && matches!(wait, PreflightWait::Busy)) =>
                {
                    return Ok(ControlFlow::Break(wait.native_reason()));
                }
                PreflightResult::Waiting(wait) => {
                    return self
                        .record_deferred(dispatch, candidate, &invocation, wait.native_reason())
                        .await
                        .map(ControlFlow::Continue);
                }
            }
        };
        let operation = self
            .reserve_operation(
                dispatch,
                candidate,
                &verified.invocation,
                &verified.snapshot.generation,
                None,
            )
            .await?;
        let mut tx = self.pool.begin().await?;
        let active = current_revision(&mut tx, dispatch.run).await?;
        // Drop the owner before releasing the policy row lock if this future
        // is cancelled while blocked in the submission backend.
        let (gate, _owner) = deadline.gate(&dispatch.admission, run_deadline);
        if !lock_permit(&mut tx, operation).await? {
            return Ok(ControlFlow::Continue(()));
        }
        let (state, attempted, reason, terminal_class) = if !active {
            ("deferred", false, Some("policy_revision_inactive".into()), None)
        } else if let ControlFlow::Break(reason) = gate.status() {
            ("deferred", false, Some(reason.reason().into()), None)
        } else if !verify_worker_permit(worker).await? {
            (
                "deferred",
                false,
                Some("permit_store_requires_synchronous_commit".into()),
                None,
            )
        } else {
            match backend
                .submit(
                    &dispatch.config.mount_path,
                    fid,
                    verified.invocation,
                    &verified.snapshot.generation,
                    gate,
                )
                .await
            {
                Submission::Accepted => ("submitted", true, None, None),
                Submission::Deferred(reason) => ("deferred", false, Some(reason.reason().into()), None),
                Submission::Rejected { error, class } => ("failed", true, Some(error), class.map(|class| class.name())),
                Submission::Unknown(error) => ("unknown", true, Some(error), None),
            }
        };
        sqlx::query(
            "UPDATE rev8_operations SET state = ?, attempted = ?, reason = ?, terminal_error_class = ?,
             accepted_at = CASE WHEN ? = 'submitted' THEN UTC_TIMESTAMP(6) ELSE NULL END,
             finished_at = CASE WHEN ? = 'failed' THEN UTC_TIMESTAMP(6) ELSE NULL END WHERE id = ? AND state = 'submitting'",
        )
        .bind(state)
        .bind(attempted)
        .bind(reason.as_deref().map(short_reason))
        .bind(terminal_class)
        .bind(state)
        .bind(state)
        .bind(operation)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ControlFlow::Continue(()))
    }
}

async fn observe_native_candidate(
    backend: Arc<dyn NativeHsm>, admission: Arc<Admission>, filesystem: FileSystemId, key: PreflightKey,
    mut invocation: Invocation, deadline: Instant,
) -> NativePreflight {
    let FidObservation::Present(snapshot) = backend.observe_in_background(&key.mount, key.fid).await? else {
        return Err("fid_missing".into());
    };
    let seen = &snapshot.generation;
    if (seen.size, seen.mtime, seen.ctime) != key.catalog_version {
        return Err("catalog_generation_changed".into());
    }
    if matches!(invocation.kind, ActionKind::Release | ActionKind::Restore) {
        invocation.archive_id = snapshot.state.archive_id;
    }
    if let Decision::Defer(reason) = admission.authorize(filesystem.as_str(), &invocation) {
        return Err(reason);
    }
    invocation.precondition(&snapshot.state)?;
    if Instant::now() >= deadline {
        return Err(PreflightWait::TimedOut.native_reason().into());
    }
    if !admission.window_open(Utc::now()) {
        return Err(SubmissionDeferral::WindowClosed.reason().into());
    }
    verify_native_candidate(backend.as_ref(), &key, invocation, snapshot).await
}

async fn verify_native_candidate(
    backend: &dyn NativeHsm, key: &PreflightKey, invocation: Invocation, snapshot: Observation,
) -> NativePreflight {
    let checked_at = Instant::now();
    let current = backend.observe_in_background(&key.mount, key.fid).await;
    if checked_at.elapsed() >= FINAL_LOOKUP_LIMIT {
        return Err(PreflightWait::TimedOut.native_reason().into());
    }
    match current {
        Ok(FidObservation::Present(current))
            if current.generation == snapshot.generation && current.state == snapshot.state =>
        {
            Ok(VerifiedNativeCandidate {
                invocation,
                snapshot: current,
                checked_at,
            })
        }
        _ => Err("target_changed_before_submit".into()),
    }
}

struct NativeDeadline {
    at: Instant,
    reason: SubmissionDeferral,
}

impl NativeDeadline {
    fn new(dispatch: &Dispatch<'_>, action: ActionKind) -> Result<Self, PolicyError> {
        let now = Instant::now();
        let policy = dispatch
            .policy
            .budget
            .duration_ms
            .ok_or_else(|| invalid("enforced native HSM requires a run duration"))?;
        let safety = dispatch
            .admission
            .safety
            .iter()
            .filter(|scope| safety_matches(scope, dispatch.policy.filesystem.as_str(), action))
            .filter_map(|scope| BudgetLimits::from_safety(scope).duration_ms)
            .min();
        let (limit, reason) = match safety {
            Some(safety) if safety < policy => (safety, SubmissionDeferral::SystemSafetyRunDuration),
            _ => (policy, SubmissionDeferral::PolicyRunDuration),
        };
        let remaining = Duration::from_millis(limit.saturating_sub(elapsed_ms(dispatch.run)));
        let at = now
            .checked_add(remaining)
            .ok_or_else(|| invalid("native HSM run duration exceeds the supported range"))?;
        Ok(Self { at, reason })
    }

    fn gate(self, admission: &Admission, retained_deadline: Instant) -> (SubmissionGate, SubmissionOwner) {
        let window_open = admission.window_check();
        SubmissionGate::owned(move || {
            if !window_open() {
                return ControlFlow::Break(SubmissionDeferral::WindowClosed);
            }
            if Instant::now() >= retained_deadline {
                return ControlFlow::Break(self.reason);
            }
            ControlFlow::Continue(())
        })
    }
}
