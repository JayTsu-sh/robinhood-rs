use std::{collections::BTreeMap, sync::Arc};

use axum::http::{HeaderMap, header::AUTHORIZATION};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::handlers::ApiError;

/// Principals permitted to write policy configuration, keyed by token digest.
#[derive(Clone, Default)]
pub struct PolicyWriters(Arc<Vec<(String, [u8; 32])>>);

impl PolicyWriters {
    /// Parse a principal-to-hex-SHA256 map from trusted server configuration.
    pub fn from_digest_json(json: &str) -> Result<Self, String> {
        let entries: BTreeMap<String, String> = serde_json::from_str(json)
            .map_err(|_| "expected a JSON map from principal names to SHA-256 digests".to_owned())?;
        let mut writers: Vec<(String, [u8; 32])> = Vec::new();
        for (principal, encoded) in entries {
            if principal.is_empty()
                || principal.len() > 255
                || principal.chars().any(|c| c.is_control() || c.is_whitespace())
            {
                return Err("invalid policy writer principal name".into());
            }
            let mut digest = [0u8; 32];
            hex::decode_to_slice(encoded, &mut digest)
                .map_err(|_| "invalid policy writer SHA-256 digest".to_owned())?;
            if writers.iter().any(|(_, existing)| existing == &digest) {
                return Err("duplicate policy writer token digest".into());
            }
            writers.push((principal, digest));
        }
        Ok(Self(Arc::new(writers)))
    }

    pub(crate) fn authorize(&self, headers: &HeaderMap) -> Result<&str, ApiError> {
        let mut values = headers.get_all(AUTHORIZATION).iter();
        let supplied = values.next().and_then(|header| header.to_str().ok());
        let Some((scheme, token)) = supplied.and_then(|value| value.split_once(' ')) else {
            return Err(ApiError::Unauthorized);
        };
        if values.next().is_some()
            || !scheme.eq_ignore_ascii_case("bearer")
            || token.is_empty()
            || token.bytes().any(|byte| byte.is_ascii_whitespace())
        {
            return Err(ApiError::Unauthorized);
        }
        let digest: [u8; 32] = Sha256::digest(token).into();
        self.0
            .iter()
            .find(|(_, expected)| bool::from(expected.ct_eq(&digest)))
            .map(|(principal, _)| principal.as_str())
            .ok_or(ApiError::Unauthorized)
    }
}
