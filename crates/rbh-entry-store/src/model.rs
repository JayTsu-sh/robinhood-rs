//! Domain types for the entry catalog.

use bytes::Bytes;
use lustre_api::LuFid;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Stable identifier for one configured filesystem.
///
/// Values are deliberately limited to a small, URL- and SQL-safe alphabet so
/// the same identifier can be used in configuration, metrics, and catalog
/// keys without separate escaping rules.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct FileSystemId(String);

impl FileSystemId {
    pub const MAX_LEN: usize = 64;

    pub fn new(value: impl Into<String>) -> Result<Self, FileSystemIdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(FileSystemIdError::Empty);
        }
        if value.len() > Self::MAX_LEN {
            return Err(FileSystemIdError::TooLong { len: value.len() });
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(FileSystemIdError::InvalidCharacter);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for FileSystemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for FileSystemId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FileSystemIdError {
    #[error("filesystem id cannot be empty")]
    Empty,
    #[error("filesystem id is {len} bytes; maximum is 64")]
    TooLong { len: usize },
    #[error("filesystem id may contain only ASCII letters, digits, '-', '_', and '.'")]
    InvalidCharacter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Lustre,
    JuiceFs,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lustre => "lustre",
            Self::JuiceFs => "juice_fs",
        }
    }

    pub fn from_persisted(value: &str) -> Result<Self, BackendKindParseError> {
        match value {
            "lustre" => Ok(Self::Lustre),
            "juice_fs" => Ok(Self::JuiceFs),
            other => Err(BackendKindParseError(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown backend kind: {0}")]
pub struct BackendKindParseError(String);

/// Operations and metadata a backend can provide.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendCapabilities {
    pub changelog: bool,
    pub namespace: bool,
    pub purge: bool,
    pub hsm: bool,
    pub stripe: bool,
    pub ost: bool,
}

/// Configuration shared by all filesystem runtimes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSystemConfig {
    pub id: FileSystemId,
    pub backend: BackendKind,
    pub mount_path: PathBuf,
    pub capabilities: BackendCapabilities,
}

/// A backend-native object identity. Variants must never be converted into
/// each other merely to fit an existing storage format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectId {
    Lustre(LuFid),
    JuiceFs(u64),
}

impl ObjectId {
    pub fn backend(self) -> BackendKind {
        match self {
            Self::Lustre(_) => BackendKind::Lustre,
            Self::JuiceFs(_) => BackendKind::JuiceFs,
        }
    }
}

/// Globally unambiguous catalog identity: filesystem plus native object id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntryKey {
    filesystem: FileSystemId,
    object: ObjectId,
}

impl EntryKey {
    pub fn new(filesystem: FileSystemId, object: ObjectId) -> Self {
        Self { filesystem, object }
    }

    pub fn filesystem(&self) -> &FileSystemId {
        &self.filesystem
    }

    pub fn object(&self) -> &ObjectId {
        &self.object
    }
}

/// An entry stored under the new filesystem-scoped identity model.
///
/// This intentionally lives beside [`EntryRow`] during the expand phase so
/// existing Lustre callers keep compiling while new adapters avoid synthetic
/// FIDs from their first persisted record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopedEntryRow {
    pub key: EntryKey,
    pub parent: Option<EntryKey>,
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
    pub depth: u32,
}

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

    #[test]
    fn entry_keys_keep_backend_native_object_ids_distinct() {
        let filesystem = FileSystemId::new("production").unwrap();
        let fid = LuFid::new(0x200000401, 0x42, 0);

        let lustre = EntryKey::new(filesystem.clone(), ObjectId::Lustre(fid));
        let juicefs = EntryKey::new(filesystem, ObjectId::JuiceFs(0x200000401));

        assert_ne!(lustre, juicefs);
        assert_eq!(lustre.object(), &ObjectId::Lustre(fid));
        assert_eq!(juicefs.object(), &ObjectId::JuiceFs(0x200000401));
    }

    #[test]
    fn filesystem_configuration_roundtrips_through_json() {
        let config = FileSystemConfig {
            id: FileSystemId::new("archive-jfs").unwrap(),
            backend: BackendKind::JuiceFs,
            mount_path: "/mnt/archive".into(),
            capabilities: BackendCapabilities {
                changelog: true,
                namespace: true,
                purge: true,
                ..BackendCapabilities::default()
            },
        };

        let json = serde_json::to_string(&config).unwrap();
        let decoded: FileSystemConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, config);
        assert!(json.contains("\"backend\":\"juice_fs\""));
    }

    #[test]
    fn filesystem_id_rejects_values_unsafe_for_persistence() {
        assert!(FileSystemId::new("").is_err());
        assert!(FileSystemId::new("contains whitespace").is_err());
        assert!(FileSystemId::new("a".repeat(65)).is_err());
        assert_eq!(FileSystemId::new("prod-01.eu").unwrap().as_str(), "prod-01.eu");
    }
}
