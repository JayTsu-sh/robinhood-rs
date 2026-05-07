//! Domain types for the entry catalog.

use bytes::Bytes;
use lustre_api::LuFid;
use serde::{Deserialize, Serialize};

/// Entry kind — matches the `kind` TINYINT column in `entries`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum EntryKind {
    File = 0,
    Directory = 1,
    Symlink = 2,
    CharDevice = 3,
    BlockDevice = 4,
    Fifo = 5,
    Socket = 6,
}

impl EntryKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::File),
            1 => Some(Self::Directory),
            2 => Some(Self::Symlink),
            3 => Some(Self::CharDevice),
            4 => Some(Self::BlockDevice),
            5 => Some(Self::Fifo),
            6 => Some(Self::Socket),
            _ => None,
        }
    }
}

/// One row in `rbh_entries.entries`. FID-keyed, Lustre/Linux-only.
/// See `entry_row_replaces_nasentry.md` for the design rationale.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryRow {
    pub fid: LuFid,
    pub parent_fid: Option<LuFid>,
    #[serde(with = "serde_bytes_compat")]
    pub name: Bytes,
    pub kind: EntryKind,
    pub size: u64,
    pub blocks: u64,
    pub uid: u32,
    pub gid: u32,
    pub projid: u32,
    pub mode: u32,
    pub nlink: u32,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
    pub stripe_count: Option<u16>,
    pub stripe_size: Option<u32>,
    pub pool_name: Option<String>,
    pub sm_status: serde_json::Value,
    pub last_seen: i64,
    /// Directory depth from filesystem root (0 = root, 1 = immediate child, …).
    /// Set to 0 for entries ingested via changelog (depth unknown without path traversal);
    /// populated correctly by the initial fs-scan.
    pub depth: u32,
}

/// Round-trip `bytes::Bytes` through serde as either a UTF-8 string or
/// a base64-encoded string when the payload isn't valid UTF-8. Lustre
/// filenames are *almost always* valid UTF-8 (NFC on Linux); the fallback
/// exists because robinhood-C is permissive.
mod serde_bytes_compat {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as B64;
    use bytes::Bytes;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Utf8(String),
        B64 { base64: String },
    }

    pub fn serialize<S: Serializer>(b: &Bytes, s: S) -> Result<S::Ok, S::Error> {
        match std::str::from_utf8(b) {
            Ok(text) => Repr::Utf8(text.to_owned()).serialize(s),
            Err(_) => Repr::B64 { base64: B64.encode(b) }.serialize(s),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Bytes, D::Error> {
        match Repr::deserialize(d)? {
            Repr::Utf8(s) => Ok(Bytes::from(s.into_bytes())),
            Repr::B64 { base64 } => B64
                .decode(base64.as_bytes())
                .map(Bytes::from)
                .map_err(serde::de::Error::custom),
        }
    }
}

/// One row in `rbh_entries.removed_entries`.
#[derive(Debug, Clone)]
pub struct RemovedEntry {
    pub fid: LuFid,
    pub parent_fid: Option<LuFid>,
    pub name: Bytes,
    pub kind: EntryKind,
    pub size: u64,
    pub uid: u32,
    pub gid: u32,
    pub sm_status: serde_json::Value,
    pub rm_time: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_row() -> EntryRow {
        EntryRow {
            fid: LuFid::new(0x200000401, 0x42, 0),
            parent_fid: Some(LuFid::new(0x200000401, 0x01, 0)),
            name: Bytes::from_static(b"hello.txt"),
            kind: EntryKind::File,
            size: 1024,
            blocks: 2,
            uid: 1000,
            gid: 100,
            projid: 0,
            mode: 0o644,
            nlink: 1,
            atime: 1,
            mtime: 2,
            ctime: 3,
            stripe_count: Some(2),
            stripe_size: Some(4 * 1024 * 1024),
            pool_name: Some("pool1".into()),
            sm_status: serde_json::json!({"hsm_state": "archived"}),
            last_seen: 4,
            depth: 0,
        }
    }

    #[test]
    fn utf8_name_roundtrips_as_string() {
        let r = demo_row();
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"name\":\"hello.txt\""), "json={s}");
        let back: EntryRow = serde_json::from_str(&s).unwrap();
        assert_eq!(back.name, r.name);
        assert_eq!(back.fid, r.fid);
        assert_eq!(back.pool_name, r.pool_name);
        assert_eq!(back.sm_status, r.sm_status);
    }

    #[test]
    fn non_utf8_name_roundtrips_as_base64() {
        let mut r = demo_row();
        r.name = Bytes::from_static(&[0xff, 0xfe, 0x00, 0x41]);
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"base64\""), "expected base64 fallback, got: {s}");
        let back: EntryRow = serde_json::from_str(&s).unwrap();
        assert_eq!(back.name, r.name);
    }
}
