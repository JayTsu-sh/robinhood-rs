//! Parse a raw changelog record into a typed [`ChangelogEvent`].
//!
//! The [`RawRec`] trait abstracts the field-access surface that
//! [`parse_event`] needs. Production code uses `lustre_api::RecView` (which
//! impls `RawRec`); unit tests use [`FakeRec`] — a plain struct that
//! implements the trait directly, avoiding byte-buffer gymnastics.

use bytes::Bytes;
use lustre_api::{ChangelogEventType, LuFid, RecView};

use crate::error::ChangelogError;
use crate::event::{ChangelogEvent, ChangelogEventEnvelope};

// ── CLF_FLAGMASK: low 12 bits of cr_flags are type-specific ─────────────
const CLF_FLAGMASK: u16 = 0x0FFF;

// ── Per-type flag bits (from lustre_user.h) ─────────────────────────────
// CL_UNLINK
const CLF_UNLINK_LAST: u16 = 0x0001;
const CLF_UNLINK_HSM_EXISTS: u16 = 0x0002;

// CL_HSM bitfield layout within the low 12 bits:
//   bits 0-6  = error code
//   bits 7-9  = hsm_event enum (archive/restore/release/remove/cancel/state)
//   bits 10-11 = CLF_HSM_DIRTY etc.
const HSM_ERR_MASK: u16 = 0x007F; // bits 0-6
const HSM_EVENT_SHIFT: u16 = 7;
const HSM_EVENT_MASK: u16 = 0x0007; // 3 bits after shift
const HSM_FLAGS_SHIFT: u16 = 10;
const HSM_FLAGS_MASK: u16 = 0x0003; // 2 bits after shift

/// Trait abstracting the fields [`parse_event`] needs from a raw changelog
/// record. This exists so unit tests can supply a [`FakeRec`] without
/// constructing packed byte buffers.
pub trait RawRec {
    fn event_type(&self) -> ChangelogEventType;
    fn index(&self) -> u64;
    /// Raw Lustre `cr_time` (`seconds << 30 | nanoseconds`).
    fn time(&self) -> u64;
    fn target_fid(&self) -> LuFid;
    fn parent_fid(&self) -> LuFid;
    fn name(&self) -> &[u8];
    fn source_name(&self) -> Option<&[u8]>;
    fn rename_source_fid(&self) -> Option<LuFid>;
    fn rename_source_parent_fid(&self) -> Option<LuFid>;
    fn uid(&self) -> Option<u32>;
    fn gid(&self) -> Option<u32>;
    fn jobid(&self) -> Option<&[u8]>;
    /// Full cr_flags value (structural + type-specific bits).
    fn flags(&self) -> u16;
}

impl RawRec for RecView<'_> {
    fn event_type(&self) -> ChangelogEventType {
        RecView::event_type(self)
    }
    fn index(&self) -> u64 {
        RecView::index(self)
    }
    fn time(&self) -> u64 {
        RecView::time(self)
    }
    fn target_fid(&self) -> LuFid {
        RecView::target_fid(self)
    }
    fn parent_fid(&self) -> LuFid {
        RecView::parent_fid(self)
    }
    fn name(&self) -> &[u8] {
        RecView::name(self)
    }
    fn source_name(&self) -> Option<&[u8]> {
        RecView::source_name(self)
    }
    fn rename_source_fid(&self) -> Option<LuFid> {
        RecView::rename_source_fid(self)
    }
    fn rename_source_parent_fid(&self) -> Option<LuFid> {
        RecView::rename_source_parent_fid(self)
    }
    fn uid(&self) -> Option<u32> {
        RecView::uid(self)
    }
    fn gid(&self) -> Option<u32> {
        RecView::gid(self)
    }
    fn jobid(&self) -> Option<&[u8]> {
        RecView::jobid(self)
    }
    fn flags(&self) -> u16 {
        RecView::flags(self)
    }
}

/// Convert a raw changelog record into a typed [`ChangelogEventEnvelope`].
///
/// Returns `Err` only for genuinely broken records (unknown rename layout,
/// missing name on a record that should have one). Unknown record types
/// are silently dropped (returns `Ok(None)`) since forward-compat Lustre
/// versions may add new types that we don't handle yet.
pub fn parse_event(mdt: &str, rec: &impl RawRec) -> Result<Option<ChangelogEventEnvelope>, ChangelogError> {
    let idx = rec.index();
    // Lustre packs changelog timestamps as seconds in the high 34 bits and
    // nanoseconds in the low 30 bits. This is the same decoding used by
    // `lfs changelog`; persisting the packed value as Unix seconds corrupts
    // removal times and all other event timestamps.
    let time = (rec.time() >> 30) as i64;
    let fid = rec.target_fid();
    let parent = rec.parent_fid();
    let name = Bytes::copy_from_slice(rec.name());
    let type_flags = rec.flags() & CLF_FLAGMASK;

    let jobid = rec.jobid().and_then(|b| {
        if b.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(b).into_owned())
        }
    });
    let uid = rec.uid();
    let gid = rec.gid();

    let event = match rec.event_type() {
        ChangelogEventType::Create => ChangelogEvent::Create {
            fid,
            parent,
            name,
            jobid,
            uid,
            gid,
        },
        ChangelogEventType::Mkdir => ChangelogEvent::Mkdir {
            fid,
            parent,
            name,
            jobid,
            uid,
            gid,
        },
        ChangelogEventType::Mknod => ChangelogEvent::Mknod {
            fid,
            parent,
            name,
            jobid,
            uid,
            gid,
        },
        ChangelogEventType::Hardlink => ChangelogEvent::Hardlink { fid, parent, name },
        ChangelogEventType::Softlink => ChangelogEvent::Softlink { fid, parent, name },
        ChangelogEventType::Unlink => {
            let last_link = type_flags & CLF_UNLINK_LAST != 0;
            let hsm_exists = type_flags & CLF_UNLINK_HSM_EXISTS != 0;
            ChangelogEvent::Unlink {
                fid,
                parent,
                name,
                last_link,
                hsm_exists,
            }
        }
        ChangelogEventType::Rmdir => ChangelogEvent::Rmdir { fid, parent, name },
        ChangelogEventType::Rename | ChangelogEventType::RenameTo => {
            // For RENAME records, cr_tfid (target_fid) is zero. The actual file
            // FID is in the rename extension's cr_sfid (source FID).
            let file_fid = rec.rename_source_fid().unwrap_or(fid);
            let src_parent = rec.rename_source_parent_fid().ok_or(ChangelogError::Parse {
                index: idx,
                reason: "rename missing source parent FID",
            })?;
            let src_name = Bytes::copy_from_slice(rec.source_name().ok_or(ChangelogError::Parse {
                index: idx,
                reason: "rename missing source name",
            })?);
            ChangelogEvent::Rename {
                fid: file_fid,
                parent,
                name,
                src_parent,
                src_name,
            }
        }
        ChangelogEventType::Close => ChangelogEvent::Close { fid, parent, name },
        ChangelogEventType::Truncate => ChangelogEvent::Truncate { fid },
        ChangelogEventType::SetAttr => ChangelogEvent::SetAttr { fid },
        ChangelogEventType::MTime => ChangelogEvent::MTime { fid },
        ChangelogEventType::CTime => ChangelogEvent::CTime { fid },
        ChangelogEventType::XAttr => {
            let xattr_name = Bytes::copy_from_slice(rec.name());
            ChangelogEvent::XAttr { fid, xattr_name }
        }
        ChangelogEventType::Layout => ChangelogEvent::Layout { fid },
        ChangelogEventType::Hsm => {
            let hsm_error = (type_flags & HSM_ERR_MASK) as u8;
            let hsm_event = ((type_flags >> HSM_EVENT_SHIFT) & HSM_EVENT_MASK) as u8;
            let hsm_flags = ((type_flags >> HSM_FLAGS_SHIFT) & HSM_FLAGS_MASK) as u8;
            ChangelogEvent::Hsm {
                fid,
                hsm_event,
                hsm_flags,
                hsm_error,
            }
        }
        // Dropped at the reader level per IGNORE_ALWAYS; but if they slip through,
        // return None so the caller can skip without error.
        ChangelogEventType::Mark
        | ChangelogEventType::Open
        | ChangelogEventType::NoOpen
        | ChangelogEventType::ATime
        | ChangelogEventType::Migrate
        | ChangelogEventType::FlrWrite
        | ChangelogEventType::Resync
        | ChangelogEventType::GetXattr
        | ChangelogEventType::Unknown(_) => return Ok(None),
    };

    Ok(Some(ChangelogEventEnvelope::new(mdt, idx, time, event)))
}

// ── Test support ─────────────────────────────────────────────────────────

/// Plain struct implementing [`RawRec`] for unit tests. Avoids the need to
/// construct real packed `changelog_rec` byte buffers.
#[derive(Debug, Clone)]
pub struct FakeRec {
    pub event_type: ChangelogEventType,
    pub index: u64,
    pub time: u64,
    pub target_fid: LuFid,
    pub parent_fid: LuFid,
    pub name: Vec<u8>,
    pub source_name: Option<Vec<u8>>,
    pub rename_source_fid: Option<LuFid>,
    pub rename_source_parent_fid: Option<LuFid>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub jobid: Option<Vec<u8>>,
    pub flags: u16,
}

impl Default for FakeRec {
    fn default() -> Self {
        Self {
            event_type: ChangelogEventType::Create,
            index: 1,
            time: 1_775_955_820 << 30,
            target_fid: LuFid::new(0x200000401, 0x1a, 0),
            parent_fid: LuFid::new(0x200000401, 0x01, 0),
            name: b"test.txt".to_vec(),
            source_name: None,
            rename_source_fid: None,
            rename_source_parent_fid: None,
            uid: None,
            gid: None,
            jobid: None,
            flags: 0,
        }
    }
}

impl RawRec for FakeRec {
    fn event_type(&self) -> ChangelogEventType {
        self.event_type
    }
    fn index(&self) -> u64 {
        self.index
    }
    fn time(&self) -> u64 {
        self.time
    }
    fn target_fid(&self) -> LuFid {
        self.target_fid
    }
    fn parent_fid(&self) -> LuFid {
        self.parent_fid
    }
    fn name(&self) -> &[u8] {
        &self.name
    }
    fn source_name(&self) -> Option<&[u8]> {
        self.source_name.as_deref()
    }
    fn rename_source_fid(&self) -> Option<LuFid> {
        self.rename_source_fid
    }
    fn rename_source_parent_fid(&self) -> Option<LuFid> {
        self.rename_source_parent_fid
    }
    fn uid(&self) -> Option<u32> {
        self.uid
    }
    fn gid(&self) -> Option<u32> {
        self.gid
    }
    fn jobid(&self) -> Option<&[u8]> {
        self.jobid.as_deref()
    }
    fn flags(&self) -> u16 {
        self.flags
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MDT: &str = "testfs-MDT0000";

    #[test]
    fn parse_create() {
        let rec = FakeRec {
            uid: Some(1000),
            gid: Some(100),
            jobid: Some(b"sh.0".to_vec()),
            ..Default::default()
        };
        let env = parse_event(MDT, &rec).unwrap().unwrap();
        assert_eq!(env.mdt, MDT);
        assert_eq!(env.index, 1);
        match &env.event {
            ChangelogEvent::Create {
                fid, uid, gid, jobid, ..
            } => {
                assert_eq!(*fid, rec.target_fid);
                assert_eq!(*uid, Some(1000));
                assert_eq!(*gid, Some(100));
                assert_eq!(jobid.as_deref(), Some("sh.0"));
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn parse_decodes_lustre_packed_timestamp_to_unix_seconds() {
        // Captured from a real Lustre 2.16 UNLINK record. Lustre stores
        // cr_time as seconds in the high 34 bits and nanoseconds in the low
        // 30 bits; it is not itself a Unix-seconds value.
        let rec = FakeRec {
            time: 1_919_724_587_777_133_588,
            ..Default::default()
        };

        let env = parse_event(MDT, &rec).unwrap().unwrap();

        assert_eq!(env.time, 1_787_882_845);
    }

    #[test]
    fn parse_unlink_with_flags() {
        let rec = FakeRec {
            event_type: ChangelogEventType::Unlink,
            flags: CLF_UNLINK_LAST | CLF_UNLINK_HSM_EXISTS,
            ..Default::default()
        };
        let env = parse_event(MDT, &rec).unwrap().unwrap();
        match &env.event {
            ChangelogEvent::Unlink {
                last_link, hsm_exists, ..
            } => {
                assert!(*last_link);
                assert!(*hsm_exists);
            }
            other => panic!("expected Unlink, got {other:?}"),
        }
    }

    #[test]
    fn parse_rename() {
        let src_parent = LuFid::new(0x200000401, 0x02, 0);
        let rec = FakeRec {
            event_type: ChangelogEventType::Rename,
            name: b"new_name.txt".to_vec(),
            source_name: Some(b"old_name.txt".to_vec()),
            rename_source_parent_fid: Some(src_parent),
            rename_source_fid: Some(LuFid::new(0x200000401, 0x1a, 0)),
            ..Default::default()
        };
        let env = parse_event(MDT, &rec).unwrap().unwrap();
        match &env.event {
            ChangelogEvent::Rename {
                name,
                src_name,
                src_parent: sp,
                ..
            } => {
                assert_eq!(name.as_ref(), b"new_name.txt");
                assert_eq!(src_name.as_ref(), b"old_name.txt");
                assert_eq!(*sp, src_parent);
            }
            other => panic!("expected Rename, got {other:?}"),
        }
    }

    #[test]
    fn parse_hsm_extracts_bitfields() {
        // Simulate: event=1 (restore), flags=1 (dirty), error=3
        // Layout: bits 0-6 = error=3, bits 7-9 = event=1 (0b001), bits 10-11 = flags=1 (0b01)
        let type_flags: u16 = 3 | (1 << 7) | (1 << 10);
        let rec = FakeRec {
            event_type: ChangelogEventType::Hsm,
            flags: type_flags,
            ..Default::default()
        };
        let env = parse_event(MDT, &rec).unwrap().unwrap();
        match &env.event {
            ChangelogEvent::Hsm {
                hsm_event,
                hsm_flags,
                hsm_error,
                ..
            } => {
                assert_eq!(*hsm_error, 3);
                assert_eq!(*hsm_event, 1);
                assert_eq!(*hsm_flags, 1);
            }
            other => panic!("expected Hsm, got {other:?}"),
        }
    }

    #[test]
    fn parse_mark_returns_none() {
        let rec = FakeRec {
            event_type: ChangelogEventType::Mark,
            ..Default::default()
        };
        assert!(parse_event(MDT, &rec).unwrap().is_none());
    }

    #[test]
    fn parse_open_returns_none() {
        let rec = FakeRec {
            event_type: ChangelogEventType::Open,
            ..Default::default()
        };
        assert!(parse_event(MDT, &rec).unwrap().is_none());
    }

    #[test]
    fn parse_unknown_type_returns_none() {
        let rec = FakeRec {
            event_type: ChangelogEventType::Unknown(99),
            ..Default::default()
        };
        assert!(parse_event(MDT, &rec).unwrap().is_none());
    }

    #[test]
    fn parse_rename_missing_source_name_errors() {
        let rec = FakeRec {
            event_type: ChangelogEventType::Rename,
            rename_source_parent_fid: Some(LuFid::new(0x200000401, 0x02, 0)),
            source_name: None, // missing
            ..Default::default()
        };
        assert!(parse_event(MDT, &rec).is_err());
    }
}
