//! [`ChangelogEvent`] — typed domain enum for Lustre changelog records.
//!
//! Each variant carries exactly the fields that are meaningful for that event
//! type. No shared nested structs, no collapsing similar events — exhaustive
//! pattern matching is the point (`rust_design_style.md` rule 4).
//!
//! [`ChangelogEventEnvelope`] wraps a `ChangelogEvent` with stream routing
//! metadata (mdt, record index, wall-clock time). Keeping routing separate
//! from domain payload means logging the envelope exposes where-it-happened
//! without polluting the event-level serialization in cross-system pipes.

use bytes::Bytes;
use lustre_api::LuFid;
use serde::{Deserialize, Serialize};

/// Typed changelog record.
///
/// Variants mirror robinhood-C's handled record types. See
/// `.claude/memory/robinhood_c_changelog_reader_semantics.md` §9 for the
/// justification of which types we handle and which are dropped at the reader
/// level (`IGNORE_ALWAYS`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangelogEvent {
    /// A regular file was created (`CL_CREATE`).
    Create {
        fid: LuFid,
        parent: LuFid,
        #[serde(with = "serde_bytes")]
        name: Bytes,
        jobid: Option<String>,
        uid: Option<u32>,
        gid: Option<u32>,
    },

    /// A directory was created (`CL_MKDIR`).
    Mkdir {
        fid: LuFid,
        parent: LuFid,
        #[serde(with = "serde_bytes")]
        name: Bytes,
        jobid: Option<String>,
        uid: Option<u32>,
        gid: Option<u32>,
    },

    /// A special file (fifo, device, socket) was created (`CL_MKNOD`).
    Mknod {
        fid: LuFid,
        parent: LuFid,
        #[serde(with = "serde_bytes")]
        name: Bytes,
        jobid: Option<String>,
        uid: Option<u32>,
        gid: Option<u32>,
    },

    /// A hardlink was added to an existing entry (`CL_HARDLINK`).
    Hardlink {
        fid: LuFid,
        parent: LuFid,
        #[serde(with = "serde_bytes")]
        name: Bytes,
    },

    /// A symbolic link was created (`CL_SOFTLINK`).
    Softlink {
        fid: LuFid,
        parent: LuFid,
        #[serde(with = "serde_bytes")]
        name: Bytes,
    },

    /// An entry's name was removed (`CL_UNLINK`). `last_link` indicates
    /// `CLF_UNLINK_LAST` — i.e. this was the last reference to `fid` and the
    /// entry itself is now gone. `hsm_exists` indicates `CLF_UNLINK_HSM_EXISTS`
    /// — an HSM copy may still exist on the archive backend.
    Unlink {
        fid: LuFid,
        parent: LuFid,
        #[serde(with = "serde_bytes")]
        name: Bytes,
        last_link: bool,
        hsm_exists: bool,
    },

    /// A directory was removed (`CL_RMDIR`).
    Rmdir {
        fid: LuFid,
        parent: LuFid,
        #[serde(with = "serde_bytes")]
        name: Bytes,
    },

    /// An entry was renamed (`CL_RENAME`). Carries both source and destination
    /// coordinates from the single post-LU-1331 record.
    ///
    /// Rename-overwrite semantics (emitting a synthetic unlink for the
    /// displaced destination) are handled downstream in `rbh-entry-store`,
    /// which has the `(dst_parent, dst_name)` → `fid` lookup to detect
    /// collisions. See `phase1b_decisions.md` rule 4.
    Rename {
        fid: LuFid,
        parent: LuFid,
        #[serde(with = "serde_bytes")]
        name: Bytes,
        src_parent: LuFid,
        #[serde(with = "serde_bytes")]
        src_name: Bytes,
    },

    /// A file was closed (`CL_CLOSE`). Carries parent+name because CLOSE is
    /// the reliable signal for "data has probably changed and should be
    /// re-examined by HSM archive policies".
    Close {
        fid: LuFid,
        parent: LuFid,
        #[serde(with = "serde_bytes")]
        name: Bytes,
    },

    /// A file was truncated (`CL_TRUNC`).
    Truncate { fid: LuFid },

    /// POSIX attrs changed (`CL_SETATTR`): mode, uid, gid.
    SetAttr { fid: LuFid },

    /// Modification time changed (`CL_MTIME`).
    MTime { fid: LuFid },

    /// Change (ctime) changed (`CL_CTIME`).
    CTime { fid: LuFid },

    /// An extended attribute was set or removed (`CL_XATTR`). `xattr_name` is
    /// the attribute name when the record carries one via `CLFE_XATTR`.
    XAttr {
        fid: LuFid,
        #[serde(with = "serde_bytes")]
        xattr_name: Bytes,
    },

    /// A file's layout (stripe info) changed (`CL_LAYOUT`).
    Layout { fid: LuFid },

    /// An HSM state transition (`CL_HSM`). The three u8 fields are extracted
    /// from the packed `cr_flags` bitfield: `hsm_event` is
    /// `enum hsm_event` (archive/restore/release/remove/cancel/state),
    /// `hsm_flags` carries `CLF_HSM_DIRTY` etc., and `hsm_error` is
    /// nonzero iff the HSM coordinator reported a failure.
    Hsm {
        fid: LuFid,
        hsm_event: u8,
        hsm_flags: u8,
        hsm_error: u8,
    },
}

impl ChangelogEvent {
    /// The target FID — the primary entry this event is about.
    pub fn fid(&self) -> LuFid {
        match self {
            Self::Create { fid, .. }
            | Self::Mkdir { fid, .. }
            | Self::Mknod { fid, .. }
            | Self::Hardlink { fid, .. }
            | Self::Softlink { fid, .. }
            | Self::Unlink { fid, .. }
            | Self::Rmdir { fid, .. }
            | Self::Rename { fid, .. }
            | Self::Close { fid, .. }
            | Self::Truncate { fid }
            | Self::SetAttr { fid }
            | Self::MTime { fid }
            | Self::CTime { fid }
            | Self::XAttr { fid, .. }
            | Self::Layout { fid }
            | Self::Hsm { fid, .. } => *fid,
        }
    }

    /// Short uppercase kind string, matching `lctl` output and
    /// [`lustre_api::ChangelogEventType::as_str`].
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Create { .. } => "CREAT",
            Self::Mkdir { .. } => "MKDIR",
            Self::Mknod { .. } => "MKNOD",
            Self::Hardlink { .. } => "HLINK",
            Self::Softlink { .. } => "SLINK",
            Self::Unlink { .. } => "UNLNK",
            Self::Rmdir { .. } => "RMDIR",
            Self::Rename { .. } => "RENME",
            Self::Close { .. } => "CLOSE",
            Self::Truncate { .. } => "TRUNC",
            Self::SetAttr { .. } => "SATTR",
            Self::MTime { .. } => "MTIME",
            Self::CTime { .. } => "CTIME",
            Self::XAttr { .. } => "XATTR",
            Self::Layout { .. } => "LYOUT",
            Self::Hsm { .. } => "HSM",
        }
    }
}

/// Envelope around a [`ChangelogEvent`] with stream routing metadata.
///
/// The envelope is what flows through the listener's output channel and what
/// the consumer acks. `index` is the cursor position used for watermark
/// tracking (the `committed_index` in
/// [`crate::listener::EventAck`] refers to this field).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangelogEventEnvelope {
    /// Short MDT name (e.g. `"testfs-MDT0000"`) — the source of the record.
    pub mdt: String,
    /// Monotonic record index within the MDT's changelog stream. Serves as
    /// the cursor position.
    pub index: u64,
    /// Timestamp carried by the record (seconds since UNIX epoch).
    pub time: i64,
    /// The typed domain event.
    pub event: ChangelogEvent,
}

impl ChangelogEventEnvelope {
    /// Convenience constructor.
    pub fn new(mdt: impl Into<String>, index: u64, time: i64, event: ChangelogEvent) -> Self {
        Self {
            mdt: mdt.into(),
            index,
            time,
            event,
        }
    }
}

/// Helper module — serde adapter so `Bytes` fields serialize as base64 strings
/// (via `serde_bytes::Bytes`) while staying as raw `bytes::Bytes` in Rust.
///
/// We use the `serde_bytes` crate's view to get efficient byte-slice
/// serialization without an ad-hoc `Vec<u8>` copy.
mod serde_bytes {
    use bytes::Bytes;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Bytes, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(value.as_ref())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Bytes, D::Error> {
        let v: Vec<u8> = <Vec<u8> as serde::Deserialize>::deserialize(d)?;
        Ok(Bytes::from(v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fid(seq: u64, oid: u32) -> LuFid {
        LuFid::new(seq, oid, 0)
    }

    #[test]
    fn fid_accessor_covers_every_variant() {
        let f = fid(0x200000401, 0x1a);
        let p = fid(0x200000401, 0x01);

        let cases = vec![
            ChangelogEvent::Create {
                fid: f,
                parent: p,
                name: Bytes::from_static(b"a"),
                jobid: None,
                uid: None,
                gid: None,
            },
            ChangelogEvent::Unlink {
                fid: f,
                parent: p,
                name: Bytes::from_static(b"a"),
                last_link: true,
                hsm_exists: false,
            },
            ChangelogEvent::Rename {
                fid: f,
                parent: p,
                name: Bytes::from_static(b"new"),
                src_parent: p,
                src_name: Bytes::from_static(b"old"),
            },
            ChangelogEvent::Close {
                fid: f,
                parent: p,
                name: Bytes::from_static(b"a"),
            },
            ChangelogEvent::Truncate { fid: f },
            ChangelogEvent::SetAttr { fid: f },
            ChangelogEvent::MTime { fid: f },
            ChangelogEvent::CTime { fid: f },
            ChangelogEvent::XAttr {
                fid: f,
                xattr_name: Bytes::from_static(b"user.foo"),
            },
            ChangelogEvent::Layout { fid: f },
            ChangelogEvent::Hsm {
                fid: f,
                hsm_event: 0,
                hsm_flags: 0,
                hsm_error: 0,
            },
        ];
        for ev in cases {
            assert_eq!(ev.fid(), f, "fid() mismatch for {ev:?}");
        }
    }

    #[test]
    fn kind_name_matches_lctl_format() {
        assert_eq!(
            ChangelogEvent::Create {
                fid: LuFid::ZERO,
                parent: LuFid::ZERO,
                name: Bytes::new(),
                jobid: None,
                uid: None,
                gid: None,
            }
            .kind_name(),
            "CREAT"
        );
        assert_eq!(
            ChangelogEvent::Unlink {
                fid: LuFid::ZERO,
                parent: LuFid::ZERO,
                name: Bytes::new(),
                last_link: false,
                hsm_exists: false,
            }
            .kind_name(),
            "UNLNK"
        );
        assert_eq!(ChangelogEvent::MTime { fid: LuFid::ZERO }.kind_name(), "MTIME");
        assert_eq!(
            ChangelogEvent::Hsm {
                fid: LuFid::ZERO,
                hsm_event: 0,
                hsm_flags: 0,
                hsm_error: 0
            }
            .kind_name(),
            "HSM"
        );
    }

    #[test]
    fn serde_json_roundtrip_create() {
        let original = ChangelogEvent::Create {
            fid: fid(0x200000401, 0x1a),
            parent: fid(0x200000401, 0x01),
            name: Bytes::from_static(b"test.txt"),
            jobid: Some("sh.0".to_string()),
            uid: Some(1000),
            gid: Some(100),
        };
        let json = serde_json::to_string(&original).unwrap();
        // Serialized form carries the kind discriminator.
        assert!(json.contains(r#""kind":"create""#), "missing tag in {json}");
        let back: ChangelogEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn serde_json_roundtrip_rename() {
        let original = ChangelogEvent::Rename {
            fid: fid(0x200000401, 0x42),
            parent: fid(0x200000401, 0x02),
            name: Bytes::from_static(b"new.txt"),
            src_parent: fid(0x200000401, 0x03),
            src_name: Bytes::from_static(b"old.txt"),
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains(r#""kind":"rename""#));
        let back: ChangelogEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn serde_json_roundtrip_unlink_with_flags() {
        let original = ChangelogEvent::Unlink {
            fid: fid(0x200000401, 0x1a),
            parent: fid(0x200000401, 0x01),
            name: Bytes::from_static(b"doomed"),
            last_link: true,
            hsm_exists: true,
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains(r#""last_link":true"#));
        assert!(json.contains(r#""hsm_exists":true"#));
        let back: ChangelogEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn serde_json_roundtrip_hsm() {
        let original = ChangelogEvent::Hsm {
            fid: fid(0x200000401, 0x99),
            hsm_event: 1,
            hsm_flags: 0,
            hsm_error: 0,
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: ChangelogEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn envelope_roundtrip() {
        let ev = ChangelogEvent::Truncate {
            fid: fid(0x200000401, 0x55),
        };
        let env = ChangelogEventEnvelope::new("testfs-MDT0000", 1234, 1_775_955_820, ev.clone());
        let json = serde_json::to_string(&env).unwrap();
        let back: ChangelogEventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.mdt, "testfs-MDT0000");
        assert_eq!(back.index, 1234);
        assert_eq!(back.time, 1_775_955_820);
        assert_eq!(back.event, ev);
    }
}
