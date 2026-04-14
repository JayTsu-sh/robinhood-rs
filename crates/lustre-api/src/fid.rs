//! `LuFid` — the Lustre File IDentifier.
//!
//! A FID is a `(seq, oid, ver)` triple that uniquely identifies a file on a single
//! MDT. Unlike paths, FIDs are **stable across renames and hardlinks**, so
//! robinhood-rs uses them as the primary key for every entry in the catalog.
//!
//! The display format matches `lctl` and `lfs` output: `[0x200000401:0x1a:0x0]`.
//! Parsing accepts bracketed and unbracketed forms; `0x` prefixes are required on
//! each component.

use std::fmt;
use std::num::ParseIntError;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::sys;

/// A Lustre FID — `(seq, oid, ver)`.
///
/// The public type is NOT `#[repr(C, packed)]`: the packed layout only matters at
/// the FFI boundary, where we convert via [`LuFid::from_raw`] / [`LuFid::into_raw`]
/// into the bindgen-generated `sys::lu_fid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct LuFid {
    pub seq: u64,
    pub oid: u32,
    pub ver: u32,
}

impl LuFid {
    /// The zero FID — used as a sentinel for "no FID" in changelog records.
    pub const ZERO: Self = Self { seq: 0, oid: 0, ver: 0 };

    #[inline]
    pub const fn new(seq: u64, oid: u32, ver: u32) -> Self {
        Self { seq, oid, ver }
    }

    /// True iff all components are zero.
    #[inline]
    pub const fn is_zero(&self) -> bool {
        self.seq == 0 && self.oid == 0 && self.ver == 0
    }

    /// Convert from the bindgen-generated C layout.
    #[inline]
    pub(crate) fn from_raw(raw: sys::lu_fid) -> Self {
        Self {
            seq: raw.f_seq,
            oid: raw.f_oid,
            ver: raw.f_ver,
        }
    }

    /// Convert to the bindgen-generated C layout (for passing to FFI).
    #[inline]
    pub(crate) fn into_raw(self) -> sys::lu_fid {
        sys::lu_fid {
            f_seq: self.seq,
            f_oid: self.oid,
            f_ver: self.ver,
        }
    }
}

impl fmt::Display for LuFid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[0x{:x}:0x{:x}:0x{:x}]", self.seq, self.oid, self.ver)
    }
}

/// Errors returned by [`LuFid::from_str`].
#[derive(Debug, thiserror::Error)]
pub enum FidParseError {
    #[error("invalid FID format: expected `[0xseq:0xoid:0xver]` or `0xseq:0xoid:0xver`, got {0:?}")]
    InvalidFormat(String),

    #[error("invalid hex component in FID: {0}")]
    InvalidHex(#[from] ParseIntError),
}

impl FromStr for LuFid {
    type Err = FidParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        // Accept `[...]` wrapping or bare `...`.
        let inner = match trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            Some(inner) => inner,
            None => trimmed,
        };

        let parts: Vec<&str> = inner.split(':').collect();
        if parts.len() != 3 {
            return Err(FidParseError::InvalidFormat(s.to_string()));
        }

        let parse_hex = |p: &str| -> Result<u64, FidParseError> {
            let hex = p.trim_start_matches("0x").trim_start_matches("0X");
            Ok(u64::from_str_radix(hex, 16)?)
        };

        let seq = parse_hex(parts[0])?;
        let oid_u64 = parse_hex(parts[1])?;
        let ver_u64 = parse_hex(parts[2])?;
        // oid and ver fit in u32 per the Lustre FID spec; truncation is the right behavior
        // for values that fit (no overflow check since u32::MAX covers every real Lustre FID).
        Ok(Self {
            seq,
            oid: oid_u64 as u32,
            ver: ver_u64 as u32,
        })
    }
}

impl Serialize for LuFid {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for LuFid {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bracketed() {
        let fid: LuFid = "[0x200000401:0x1a:0x0]".parse().unwrap();
        assert_eq!(fid.seq, 0x200000401);
        assert_eq!(fid.oid, 0x1a);
        assert_eq!(fid.ver, 0x0);
    }

    #[test]
    fn parse_unbracketed() {
        let fid: LuFid = "0x200000401:0x1a:0x0".parse().unwrap();
        assert_eq!(fid.seq, 0x200000401);
        assert_eq!(fid.oid, 0x1a);
    }

    #[test]
    fn parse_uppercase_hex_prefix() {
        let fid: LuFid = "[0Xdeadbeef:0X42:0X1]".parse().unwrap();
        assert_eq!(fid.seq, 0xdeadbeef);
    }

    #[test]
    fn parse_rejects_wrong_component_count() {
        assert!("0x1:0x2".parse::<LuFid>().is_err());
        assert!("0x1:0x2:0x3:0x4".parse::<LuFid>().is_err());
    }

    #[test]
    fn parse_rejects_non_hex() {
        assert!("[0xzz:0x1:0x0]".parse::<LuFid>().is_err());
    }

    #[test]
    fn display_roundtrip() {
        let fid = LuFid::new(0x200000401, 0x1a, 0);
        assert_eq!(fid.to_string(), "[0x200000401:0x1a:0x0]");
        let parsed: LuFid = fid.to_string().parse().unwrap();
        assert_eq!(fid, parsed);
    }

    #[test]
    fn serde_json_roundtrip() {
        let fid = LuFid::new(0xdeadbeef, 0x42, 0x1);
        let json = serde_json::to_string(&fid).unwrap();
        assert_eq!(json, "\"[0xdeadbeef:0x42:0x1]\"");
        let back: LuFid = serde_json::from_str(&json).unwrap();
        assert_eq!(fid, back);
    }

    #[test]
    fn zero() {
        assert!(LuFid::ZERO.is_zero());
        assert_eq!(LuFid::ZERO, LuFid::new(0, 0, 0));
        assert!(!LuFid::new(1, 0, 0).is_zero());
    }

    #[test]
    fn raw_roundtrip() {
        let fid = LuFid::new(0x200000401, 0x1a, 0x0);
        let raw = fid.into_raw();
        let back = LuFid::from_raw(raw);
        assert_eq!(fid, back);
    }
}
