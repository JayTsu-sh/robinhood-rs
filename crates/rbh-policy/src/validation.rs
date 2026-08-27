use rbh_entry_store::{BackendCapabilities, BackendKind, FileSystemConfig};

use crate::model::{ThresholdTarget, TriggerSpec};
use crate::{PolicyDef, PolicyError, PolicyKind, TargetFilter};

fn require(
    config: &FileSystemConfig, available: bool, capability: &'static str, reason: &'static str,
) -> Result<(), PolicyError> {
    if available {
        Ok(())
    } else {
        Err(PolicyError::UnsupportedCapability {
            filesystem: config.id.clone(),
            capability,
            reason,
        })
    }
}

/// Validate a persisted policy against the capabilities of its one selected
/// filesystem. Call this before creating any scheduler schedule.
pub fn validate_policy_for_filesystem(def: &PolicyDef, config: &FileSystemConfig) -> Result<(), PolicyError> {
    if def.filesystem != config.id {
        return Err(PolicyError::FilesystemMismatch {
            policy: def.filesystem.clone(),
            configured: config.id.clone(),
        });
    }
    match def.kind {
        PolicyKind::Purge => require(
            config,
            config.capabilities.purge,
            "purge",
            "purge requires a filesystem with namespace deletion enabled",
        ),
        PolicyKind::HsmArchive | PolicyKind::HsmRelease | PolicyKind::HsmRestore | PolicyKind::HsmRemove => require(
            config,
            config.capabilities.hsm,
            "hsm",
            "HSM actions require a Lustre filesystem with HSM enabled",
        ),
        _ => Ok(()),
    }?;
    let trigger = crate::parse_trigger(&def.trigger).map_err(|error| PolicyError::InvalidTrigger(error.to_string()))?;
    match trigger {
        TriggerSpec::ThresholdOstPct { target, .. } => {
            require(
                config,
                config.capabilities.ost,
                "ost",
                "OST utilization triggers are available only on Lustre",
            )?;
            validate_threshold_target(&target, config)
        }
        TriggerSpec::ThresholdFsPct { .. } => require(
            config,
            config.capabilities.ost,
            "ost",
            "filesystem utilization currently requires Lustre OST usage",
        ),
        TriggerSpec::ThresholdCount { target, .. } | TriggerSpec::ThresholdVolume { target, .. } => {
            validate_threshold_target(&target, config)
        }
        _ => Ok(()),
    }?;
    require(
        config,
        config.backend == BackendKind::Lustre || def.kind == PolicyKind::Purge,
        "action_backend",
        "this action has no JuiceFS backend adapter; only purge is currently supported",
    )
}

fn validate_threshold_target(target: &ThresholdTarget, config: &FileSystemConfig) -> Result<(), PolicyError> {
    match target {
        ThresholdTarget::Ost { .. } => require(
            config,
            config.capabilities.ost,
            "ost",
            "OST targeting is available only on Lustre",
        ),
        ThresholdTarget::Pool { .. } => require(
            config,
            config.capabilities.stripe,
            "stripe",
            "pool targeting requires backend stripe/pool metadata",
        ),
        _ => Ok(()),
    }
}

/// Validate run-time narrowing before an immediate/threshold task is queued.
pub fn validate_target_for_filesystem(target: &TargetFilter, config: &FileSystemConfig) -> Result<(), PolicyError> {
    let BackendCapabilities { ost, stripe, .. } = config.capabilities;
    match target {
        TargetFilter::Ost { .. } => require(config, ost, "ost", "OST targeting is available only on Lustre"),
        TargetFilter::Pool { .. } => require(
            config,
            stripe,
            "stripe",
            "pool targeting requires backend stripe/pool metadata",
        ),
        _ => Ok(()),
    }
}
