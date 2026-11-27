//! Standalone endpoint on the host that owns the business Lustre mount.

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    routing::post,
};
use rbh_policy::rev8::heat::agent::{MAX_BATCH_SIZE, SampleRequest, SampleResult, sample_mount, valid_token};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;

#[derive(Clone)]
struct Agent {
    mount: PathBuf,
    bearer_digest: [u8; 32],
    blocking_slot: Arc<Semaphore>,
}

#[derive(Debug, thiserror::Error)]
pub enum HeatAgentError {
    #[error("{0}")]
    Config(&'static str),
    #[error("could not read RBH_HEAT_AGENT_TOKEN_FILE: {0}")]
    Token(#[source] std::io::Error),
    #[error("heat agent HTTP server failed: {0}")]
    Server(#[from] std::io::Error),
}

#[tracing::instrument]
pub async fn run_heat_agent() -> Result<(), HeatAgentError> {
    use HeatAgentError::Config;

    let mount =
        PathBuf::from(std::env::var_os("RBH_HEAT_AGENT_MOUNT").ok_or(Config("RBH_HEAT_AGENT_MOUNT is required"))?);
    if !mount.is_absolute() {
        return Err(Config("RBH_HEAT_AGENT_MOUNT must be an absolute path"));
    }
    let address: SocketAddr = std::env::var("RBH_HEAT_AGENT_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:9184".into())
        .parse()
        .map_err(|_| Config("invalid RBH_HEAT_AGENT_LISTEN"))?;
    if !address.ip().is_loopback() {
        return Err(Config("RBH_HEAT_AGENT_LISTEN must be a loopback address"));
    }
    let token_file =
        std::env::var_os("RBH_HEAT_AGENT_TOKEN_FILE").ok_or(Config("RBH_HEAT_AGENT_TOKEN_FILE is required"))?;
    let token = tokio::fs::read_to_string(token_file)
        .await
        .map_err(HeatAgentError::Token)?;
    let token = token.trim();
    if !valid_token(token) {
        return Err(Config("invalid heat agent bearer token"));
    }
    let agent = Agent {
        mount,
        bearer_digest: Sha256::digest(token).into(),
        blocking_slot: Arc::new(Semaphore::new(1)),
    };
    let server = Router::new()
        .route("/sample", post(sample))
        .layer(DefaultBodyLimit::max(32 * 1024))
        .with_state(agent);
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, server).await?;
    Ok(())
}

async fn sample(
    State(agent): State<Agent>, headers: HeaderMap, Json(body): Json<SampleRequest>,
) -> Result<Json<SampleResult>, StatusCode> {
    let mut auth = headers.get_all(AUTHORIZATION).iter();
    let supplied = auth
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| valid_token(token))
        .filter(|_| auth.next().is_none())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let digest: [u8; 32] = Sha256::digest(supplied).into();
    if !bool::from(digest.ct_eq(&agent.bearer_digest)) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if body.fids.is_empty()
        || body.fids.len() > usize::from(MAX_BATCH_SIZE)
        || body.fids.iter().any(|fid| fid.is_zero())
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let permit = agent
        .blocking_slot
        .try_acquire_owned()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let samples = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        sample_mount(&agent.mount, &body)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(samples))
}
