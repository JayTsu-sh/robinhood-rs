//! JuiceFS identity adapter for the backend-neutral POSIX walker.

use rbh_entry_store::model::{EntryKey, FileSystemId, ObjectId, ScopedEntryRow, ScopedNamespaceEdge};

use crate::PosixEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamespaceDifference {
    pub missing_from_catalog: usize,
    pub missing_from_mount: usize,
}

/// Compare independently collected mounted and catalog namespace snapshots.
pub fn compare_namespace(
    mounted: &[ScopedNamespaceEdge], catalog: &[ScopedNamespaceEdge],
) -> Result<(), NamespaceDifference> {
    use std::collections::HashSet;
    let mounted: HashSet<_> = mounted.iter().collect();
    let catalog: HashSet<_> = catalog.iter().collect();
    let difference = NamespaceDifference {
        missing_from_catalog: mounted.difference(&catalog).count(),
        missing_from_mount: catalog.difference(&mounted).count(),
    };
    if difference.missing_from_catalog == 0 && difference.missing_from_mount == 0 {
        Ok(())
    } else {
        Err(difference)
    }
}

/// Convert one mounted JuiceFS stat into its inode object and namespace edge.
/// The root has no edge; every non-root entry must carry its parent's inode.
pub fn adapt(
    filesystem: &FileSystemId, entry: &PosixEntry, observed_at: i64,
) -> Result<(ScopedEntryRow, Option<ScopedNamespaceEdge>), &'static str> {
    let object = ObjectId::JuiceFs(entry.inode);
    let parent = entry.parent_inode.map(ObjectId::JuiceFs);
    if entry.depth > 0 && parent.is_none() {
        return Err("non-root JuiceFS entry has no parent inode");
    }
    let row = ScopedEntryRow {
        key: EntryKey::new(filesystem.clone(), object),
        parent: parent.map(|value| EntryKey::new(filesystem.clone(), value)),
        name: entry.name.clone(),
        kind: entry.kind,
        size: entry.size,
        blocks: entry.blocks,
        uid: entry.uid,
        gid: entry.gid,
        projid: 0,
        mode: entry.mode,
        nlink: entry.nlink,
        atime: entry.atime,
        mtime: entry.mtime,
        ctime: entry.ctime,
        stripe_count: None,
        stripe_size: None,
        pool_name: None,
        sm_status: serde_json::Value::Null,
        last_seen: observed_at,
        depth: entry.depth,
    };
    let edge = parent.map(|parent| ScopedNamespaceEdge {
        filesystem: filesystem.clone(),
        parent,
        name: entry.name.clone(),
        object,
    });
    Ok((row, edge))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use bytes::Bytes;
    use rbh_entry_store::model::EntryKind;

    use super::*;

    #[test]
    fn preserves_native_inode_and_parent_name_edge() {
        let filesystem = FileSystemId::new("juice").unwrap();
        let entry = PosixEntry {
            path: PathBuf::from("/jfs/a"),
            parent_path: Some(PathBuf::from("/jfs")),
            name: Bytes::from_static(b"a"),
            kind: EntryKind::File,
            device: 1,
            inode: 42,
            parent_inode: Some(1),
            size: 3,
            blocks: 8,
            uid: 1000,
            gid: 1000,
            mode: 0o100644,
            nlink: 2,
            atime: 10,
            mtime: 11,
            ctime: 12,
            depth: 1,
        };
        let (row, edge) = adapt(&filesystem, &entry, 99).unwrap();
        assert_eq!(row.key.object(), &ObjectId::JuiceFs(42));
        assert_eq!(row.parent.as_ref().unwrap().object(), &ObjectId::JuiceFs(1));
        assert_eq!(edge.unwrap().name, Bytes::from_static(b"a"));
    }

    #[test]
    fn independent_namespace_comparison_reports_both_directions() {
        let filesystem = FileSystemId::new("juice").unwrap();
        let edge = |name: &'static [u8], inode| ScopedNamespaceEdge {
            filesystem: filesystem.clone(),
            parent: ObjectId::JuiceFs(1),
            name: Bytes::from_static(name),
            object: ObjectId::JuiceFs(inode),
        };
        let difference = compare_namespace(&[edge(b"mounted", 2)], &[edge(b"catalog", 3)]).unwrap_err();
        assert_eq!(difference.missing_from_catalog, 1);
        assert_eq!(difference.missing_from_mount, 1);
    }
}
