use std::{collections::BTreeMap, sync::Arc};

use axum::http::{
    HeaderMap,
    header::{AUTHORIZATION, HOST, ORIGIN},
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::handlers::ApiError;

/// Authorization for policy, run, and filesystem management changes.
#[derive(Clone, Default)]
pub struct PolicyWriters {
    digests: Arc<Vec<(String, [u8; 32])>>,
    trusted_gateway: bool,
}

impl PolicyWriters {
    /// Configure a writer credential shared by the daemon and the trusted gateway.
    pub fn trusted_gateway(token: &str) -> Result<Self, String> {
        if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("expected a 64-digit hexadecimal gateway token".into());
        }
        Ok(Self {
            digests: Arc::new(vec![("trusted-gateway".into(), Sha256::digest(token).into())]),
            trusted_gateway: true,
        })
    }

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
        Ok(Self {
            digests: Arc::new(writers),
            trusted_gateway: false,
        })
    }

    pub(crate) fn authorize(&self, headers: &HeaderMap) -> Result<&str, ApiError> {
        if self.trusted_gateway && !is_same_origin_or_non_browser(headers) {
            return Err(ApiError::Unauthorized);
        }
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
        self.digests
            .iter()
            .find(|(_, expected)| bool::from(expected.ct_eq(&digest)))
            .map(|(principal, _)| principal.as_str())
            .ok_or(ApiError::Unauthorized)
    }
}

// Trusted network access also needs a browser origin check because browsers can
// send requests from unrelated sites while connected to that network.
fn is_same_origin_or_non_browser(headers: &HeaderMap) -> bool {
    if headers
        .get_all("sec-fetch-site")
        .iter()
        .any(|value| value != "same-origin")
    {
        return false;
    }
    let mut origins = headers.get_all(ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return true;
    };
    if origins.next().is_some() {
        return false;
    }
    let Some(host) = headers.get(HOST).and_then(|host| host.to_str().ok()) else {
        return false;
    };
    origin.to_str().ok().and_then(|origin| {
        origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
    }) == Some(host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn trusted_gateway_accepts_same_origin_and_non_browser_writes() {
        let token = "a".repeat(64);
        let writers = PolicyWriters::trusted_gateway(&token).unwrap();
        let mut headers = HeaderMap::new();
        assert!(writers.authorize(&headers).is_err());
        headers.insert(AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        assert_eq!(writers.authorize(&headers).unwrap(), "trusted-gateway");
        headers.insert(HOST, HeaderValue::from_static("console.local:8080"));
        headers.insert(ORIGIN, HeaderValue::from_static("http://console.local:8080"));
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        assert_eq!(writers.authorize(&headers).unwrap(), "trusted-gateway");

        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(writers.authorize(&headers).is_err());
        headers.remove("sec-fetch-site");
        headers.insert(ORIGIN, HeaderValue::from_static("http://console.local"));
        assert!(writers.authorize(&headers).is_err());
        headers.insert(ORIGIN, HeaderValue::from_static("null"));
        assert!(writers.authorize(&headers).is_err());
    }
}
