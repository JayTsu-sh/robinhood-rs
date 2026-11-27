//! Transport for sampling business mounts reachable through a separate node.

use std::{
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, TimeDelta, Utc};
use lustre_api::{
    LuFid,
    heat::{Decay, HeatMount, MountIdentity, ReadHeat},
};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};

pub const MAX_BATCH_SIZE: u16 = 256;
const MAX_REPLY_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AgentConfig {
    endpoint: String,
    token_file: PathBuf,
}

impl AgentConfig {
    pub(super) fn validate(&self) -> bool {
        let Ok(url) = Url::parse(&self.endpoint) else {
            return false;
        };
        let loopback = url.host_str().is_some_and(|host| {
            host.trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        });
        url.host_str().is_some()
            && (url.scheme() == "https" || (url.scheme() == "http" && loopback))
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && self.token_file.is_absolute()
    }

    pub(super) async fn sample(&self, client: &Client, batch: &SampleRequest) -> SampleResult {
        let token = tokio::fs::read_to_string(&self.token_file)
            .await
            .map_err(|_| HeatFailure::ClientAgentUnavailable)?;
        let token = token.trim();
        if !valid_token(token) {
            return Err(HeatFailure::ClientAgentUnavailable);
        }
        let started = Utc::now();
        let mut response = client
            .post(&self.endpoint)
            .bearer_auth(token)
            .json(batch)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|_| HeatFailure::ClientAgentUnavailable)?;
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| HeatFailure::ClientAgentUnavailable)?
        {
            if body.len() + chunk.len() > MAX_REPLY_BYTES {
                return Err(HeatFailure::ClientAgentUnavailable);
            }
            body.extend_from_slice(&chunk);
        }
        let result: SampleResult = serde_json::from_slice(&body).map_err(|_| HeatFailure::ClientAgentUnavailable)?;
        let mut report = result?;
        if report.readings.len() != batch.fids.len()
            || report
                .readings
                .iter()
                .zip(&batch.fids)
                .any(|(reading, fid)| reading.fid != *fid)
        {
            return Err(HeatFailure::ClientAgentUnavailable);
        }
        // The authenticated agent's sample must come from the request window.
        // Allow small clock offsets without letting future-dated rows prevent
        // later failures or valid samples from replacing them.
        let received = Utc::now().max(started);
        let tolerance = TimeDelta::seconds(2);
        for reading in &mut report.readings {
            if reading.observed_at < started - tolerance || reading.observed_at > received + tolerance {
                return Err(HeatFailure::ClientClockSkew);
            }
            reading.observed_at = reading.observed_at.clamp(started, received);
        }
        Ok(report)
    }
}

pub(super) fn client() -> Result<Client, reqwest::Error> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
}

pub fn valid_token(token: &str) -> bool {
    (32..=256).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_graphic())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SampleRequest {
    pub source: String,
    pub fids: Vec<LuFid>,
}

pub type SampleResult = Result<HeatBatch, HeatFailure>;

#[derive(Deserialize, Serialize)]
pub struct HeatBatch {
    pub(super) mount: MountIdentity,
    pub(super) decay: Decay,
    pub(super) readings: Vec<Reading>,
}

#[derive(Deserialize, Serialize)]
pub(super) struct Reading {
    pub(super) fid: LuFid,
    pub(super) observed_at: DateTime<Utc>,
    pub(super) heat: Result<ReadHeat, HeatFailure>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HeatFailure {
    ClientAgentUnavailable,
    ClientClockSkew,
    ExecutorMountUnavailable,
    ClientMountUnavailable,
    ClientMountNotIsolated,
    ClientFilesystemMismatch,
    ClientParametersUnavailable,
    ClientHeatDisabled,
    ClientParametersChanged,
    FidHeatUnavailable,
    FidSampleFailed,
}

impl HeatFailure {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::ClientAgentUnavailable => "client_agent_unavailable",
            Self::ClientClockSkew => "client_clock_skew",
            Self::ExecutorMountUnavailable => "executor_mount_unavailable",
            Self::ClientMountUnavailable => "client_mount_unavailable",
            Self::ClientMountNotIsolated => "client_mount_not_isolated",
            Self::ClientFilesystemMismatch => "client_filesystem_mismatch",
            Self::ClientParametersUnavailable => "client_parameters_unavailable",
            Self::ClientHeatDisabled => "client_heat_disabled",
            Self::ClientParametersChanged => "client_parameters_changed",
            Self::FidHeatUnavailable => "fid_heat_unavailable",
            Self::FidSampleFailed => "fid_sample_failed",
        }
    }
}

/// Called on the host where the business reads take place. Used for in-process
/// mounts and in the standalone agent, with no file data reads in either mode.
pub fn sample_mount(path: &Path, request: &SampleRequest) -> SampleResult {
    let mount = HeatMount::open(path).map_err(|_| HeatFailure::ClientMountUnavailable)?;
    if mount.identity().source != request.source {
        return Err(HeatFailure::ClientFilesystemMismatch);
    }
    let decay = mount
        .decay()
        .map_err(|_| HeatFailure::ClientParametersUnavailable)?
        .ok_or(HeatFailure::ClientHeatDisabled)?;
    let readings = request
        .fids
        .iter()
        .map(|&fid| {
            let heat = match mount.sample(fid) {
                Ok(Some(heat)) => Ok(heat),
                Ok(None) => Err(HeatFailure::FidHeatUnavailable),
                Err(error) => {
                    tracing::debug!(%fid, %error, "Lustre read heat unavailable");
                    Err(HeatFailure::FidSampleFailed)
                }
            };
            Reading {
                fid,
                observed_at: Utc::now(),
                heat,
            }
        })
        .collect();
    if !matches!(mount.decay(), Ok(Some(after)) if after == decay) {
        return Err(HeatFailure::ClientParametersChanged);
    }
    Ok(HeatBatch {
        mount: mount.identity().clone(),
        decay,
        readings,
    })
}
