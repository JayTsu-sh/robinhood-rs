use std::{
    ops::ControlFlow,
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::Utc;
use rbh_entry_store::FileSystemId;
use sqlx::{MySql, MySqlExecutor, Transaction};
use tokio::{sync::oneshot, task::JoinHandle};

use super::{
    BATCH_SIZE, Candidate, Dispatch, PolicyStore, current_revision, decode_fid, elapsed_ms, lock_permit,
    preflight::{PreflightKey, PreflightRequest, PreflightResult, PreflightWait},
    short_reason, verify_worker_permit,
};
use crate::{
    PolicyError, Rev8TickTask,
    rev8::{
        ActionKind, BudgetLimits, Invocation,
        execution::{
            Admission, Decision, FidObservation, NativeHsm, NativeSubmissionSlot, Observation, Submission,
            SubmissionDeferral, SubmissionGate, SubmissionOwner,
        },
        invalid,
        resources::safety_matches,
    },
};

// Give a fresh verification its own bounded wait after the initial reads
// crossed a turn. A verification slower than this cannot authorize an ioctl.
const FINAL_LOOKUP_LIMIT: Duration = Rev8TickTask::INTERVAL.saturating_mul(2);
const SUBMISSION_WAIT_LIMIT: Duration = Rev8TickTask::INTERVAL.saturating_mul(2);
const SUBMISSION_PENDING: &str = "native_hsm_submission_pending";

pub(in crate::store) type NativePreflight = Result<VerifiedNativeCandidate, String>;

pub(in crate::store) struct VerifiedNativeCandidate {
    invocation: Invocation,
    snapshot: Observation,
    checked_at: Instant,
}

pub(in crate::store) enum NativeSubmissionJob {
    Running(JoinHandle<Submission>),
    Finished(Submission),
}

impl Drop for NativeSubmissionJob {
    fn drop(&mut self) {
        if let Self::Running(task) = self {
            task.abort();
        }
    }
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
        let Some(slot) = NativeSubmissionSlot::acquire(&verified.snapshot.generation.filesystem_name) else {
            let reason = SubmissionDeferral::ActuatorBusy.reason();
            if dispatch.policy.one_shot {
                return Ok(ControlFlow::Break(reason));
            }
            return self
                .record_deferred(dispatch, candidate, &invocation, reason)
                .await
                .map(ControlFlow::Continue);
        };
        let mut tx = self.pool.begin().await?;
        let active = current_revision(&mut tx, dispatch.run).await?;
        // Revoke the gate before releasing the locked policy revision.
        let (gate, owner) = deadline.gate(&dispatch.admission, run_deadline);
        let gate = gate.holding(slot);
        let early_deferral = if !active {
            Some(SubmissionDeferral::PolicyInactive)
        } else if let ControlFlow::Break(reason) = gate.status() {
            Some(reason)
        } else {
            None
        };
        if let Some(reason) = early_deferral {
            tx.commit().await?;
            return self
                .record_deferred(dispatch, candidate, &invocation, reason.reason())
                .await
                .map(ControlFlow::Continue);
        }

        let mut jobs = self.native_submissions.lock().await;
        if jobs.len() >= BATCH_SIZE {
            return Ok(ControlFlow::Break(SubmissionDeferral::ActuatorBusy.reason()));
        }
        let mut intent = self.pool.begin().await?;
        let operation = self
            .reserve_operation(
                dispatch,
                candidate,
                &verified.invocation,
                &verified.snapshot.generation,
                None,
                &mut *intent,
            )
            .await?;
        let backend = Arc::clone(backend);
        let mount = dispatch.config.mount_path.clone();
        let (allow, permit) = oneshot::channel();
        let task = tokio::spawn(async move {
            let Ok(gate) = permit.await else {
                return Submission::Deferred(SubmissionDeferral::DispatchCancelled);
            };
            backend
                .submit(&mount, fid, verified.invocation, &verified.snapshot.generation, gate)
                .await
        });
        let NativeSubmissionJob::Running(task) = jobs.entry(operation).or_insert(NativeSubmissionJob::Running(task))
        else {
            unreachable!("the operation was just reserved");
        };
        // The job can report cancellation even if the commit's outcome is
        // ambiguous. The kernel request cannot start until the intent commits.
        intent.commit().await?;
        if !lock_permit(&mut tx, operation).await? {
            jobs.remove(&operation);
            return Ok(ControlFlow::Continue(()));
        }

        let mut pending = false;
        let mut yielded = None;
        let submission = if !verify_worker_permit(worker).await? {
            Submission::Deferred(SubmissionDeferral::PermitStoreNotSynchronous)
        } else {
            let wait_until = run_deadline.min(Instant::now() + SUBMISSION_WAIT_LIMIT);
            allow
                .send(gate)
                .map_err(|_| PolicyError::Store("native HSM submission worker stopped before dispatch".into()))?;
            match tokio::time::timeout_at(wait_until.into(), task).await {
                Ok(Ok(submission)) => submission,
                Ok(Err(error)) => Submission::Unknown(error.to_string()),
                Err(_) => {
                    let claimed = owner.revoke_unless_claimed();
                    if claimed {
                        pending = true;
                        if wait_until < run_deadline {
                            yielded = Some(SUBMISSION_PENDING);
                        }
                        Submission::Unknown(SUBMISSION_PENDING.into())
                    } else {
                        let reason = if !dispatch.admission.window_open(Utc::now()) {
                            SubmissionDeferral::WindowClosed
                        } else if wait_until == run_deadline {
                            deadline.reason
                        } else {
                            yielded = Some(SubmissionDeferral::TickBudget.reason());
                            SubmissionDeferral::TickBudget
                        };
                        Submission::Deferred(reason)
                    }
                }
            }
        };
        if !pending && let Some(job) = jobs.get_mut(&operation) {
            *job = NativeSubmissionJob::Finished(submission.clone());
        }
        record_submission(&mut *tx, operation, &submission, JournalUpdate::Dispatch).await?;
        tx.commit().await?;
        if !pending {
            jobs.remove(&operation);
        }
        if let Some(reason) = yielded {
            return Ok(ControlFlow::Break(reason));
        }
        Ok(ControlFlow::Continue(()))
    }

    pub(super) async fn finish_native_submissions(&self) {
        let mut active = self.native_submissions.lock().await;
        let mut committed = Vec::new();
        for (&operation, job) in active.iter_mut() {
            if let NativeSubmissionJob::Running(task) = job {
                if !task.is_finished() {
                    continue;
                }
                let result = task
                    .await
                    .unwrap_or_else(|error| Submission::Unknown(error.to_string()));
                *job = NativeSubmissionJob::Finished(result);
            }
            let NativeSubmissionJob::Finished(submission) = job else {
                unreachable!("a finished task has a submission result");
            };
            match self.publish_late_submission(operation, submission).await {
                Ok(()) => committed.push(operation),
                Err(error) => {
                    tracing::warn!(operation_id = operation, %error, "will retry recording HSM submission result");
                }
            }
        }
        for operation in committed {
            active.remove(&operation);
        }
    }

    async fn publish_late_submission(&self, operation: u64, submission: &Submission) -> Result<(), PolicyError> {
        let mut tx = self.pool.begin().await?;
        record_submission(&mut *tx, operation, submission, JournalUpdate::LateResponse).await?;
        if matches!(submission, Submission::Accepted) {
            // A recovery worker may already have confirmed the target
            // state. Preserve that evidence and add actual acceptance.
            sqlx::query(
                "UPDATE rev8_operations SET accepted_at = UTC_TIMESTAMP(6)
                 WHERE id = ? AND state = 'completed' AND accepted_at IS NULL",
            )
            .bind(operation)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

enum JournalUpdate {
    Dispatch,
    LateResponse,
}

async fn record_submission(
    store: impl MySqlExecutor<'_>, operation: u64, submission: &Submission, update: JournalUpdate,
) -> Result<(), PolicyError> {
    let (state, attempted, reason, terminal_class) = match submission {
        Submission::Accepted => ("submitted", true, None, None),
        Submission::Deferred(reason) => ("deferred", false, Some(reason.reason()), None),
        Submission::Rejected { error, class } => {
            ("failed", true, Some(error.as_str()), class.map(|class| class.name()))
        }
        Submission::Unknown(error) => ("unknown", true, Some(error.as_str()), None),
    };
    sqlx::query(
        "UPDATE rev8_operations SET state = ?, attempted = ?, reason = ?, terminal_error_class = ?,
             accepted_at = CASE WHEN ? = 'submitted' THEN UTC_TIMESTAMP(6) ELSE NULL END,
             finished_at = CASE WHEN ? = 'failed' THEN UTC_TIMESTAMP(6) ELSE NULL END
             WHERE id = ? AND (state = 'submitting'
                OR (? AND state = 'unknown' AND reason IN (?, 'interrupted_submission')))",
    )
    .bind(state)
    .bind(attempted)
    .bind(reason.map(short_reason))
    .bind(terminal_class)
    .bind(state)
    .bind(state)
    .bind(operation)
    .bind(matches!(update, JournalUpdate::LateResponse))
    .bind(SUBMISSION_PENDING)
    .execute(store)
    .await?;
    Ok(())
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

    fn gate(&self, admission: &Admission, retained_deadline: Instant) -> (SubmissionGate, SubmissionOwner) {
        let window_open = admission.window_check();
        let reason = self.reason;
        SubmissionGate::owned(move || {
            if !window_open() {
                return ControlFlow::Break(SubmissionDeferral::WindowClosed);
            }
            if Instant::now() >= retained_deadline {
                return ControlFlow::Break(reason);
            }
            ControlFlow::Continue(())
        })
    }
}
