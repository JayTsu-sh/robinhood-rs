use std::{
    fs,
    os::unix::fs::{MetadataExt, symlink},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use lustre_api::migration::{FileVersion, MigrationLimits, MigrationOwnership, MigrationPlan};
use serde_json::json;

use super::*;

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove isolated Lustre fixture");
    }
}

fn scoped_to(mount: &Path, root: &Path, excluded: &Path) -> Arc<DirectoryScope> {
    let relative = |path: &Path| format!("/{}", path.strip_prefix(mount).unwrap().display());
    let document: ScopeDocument = serde_json::from_value(json!({
        "root": relative(root),
        "excludedSubtrees": [relative(excluded)],
    }))
    .unwrap();
    Arc::new(DirectoryScope::try_from(document).unwrap())
}

#[test]
#[ignore = "requires an isolated Lustre fixture under the hot pool"]
fn directory_and_link_changes_during_copy_preserve_the_source_layout() {
    let mount = PathBuf::from(std::env::var_os("RBH_TEST_LUSTRE_MOUNT").unwrap());
    let hot = PathBuf::from(std::env::var_os("RBH_TEST_MIGRATION_HOT_DIR").unwrap());
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let root = hot.join(format!("rbh-scope-{}-{timestamp}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let fixture = Fixture(root);
    let filesystem = LustreApi.hsm_filesystem(&mount).unwrap();
    for (scenario, reason) in [
        ("outside", "link_outside_scope"),
        ("excluded", "link_in_excluded_subtree"),
        ("rename", "link_evidence_changed"),
        ("incomplete", "incomplete_link_evidence"),
    ] {
        let root = fixture.0.join(scenario);
        let team = root.join("team");
        let project = team.join("project");
        let archive = team.join("archive");
        let alias = root.join("mount-alias");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir(&archive).unwrap();
        symlink(&mount, &alias).unwrap();

        let file = team.join("file");
        let link = project.join("link");
        let bytes = vec![42; 4096];
        fs::write(&file, &bytes).unwrap();
        fs::hard_link(&file, &link).unwrap();
        let fid = LustreApi.path_to_fid(&file).unwrap();
        let original = LustreApi.migration_source_layout(&mount, &fid).unwrap();
        assert_eq!(original.pool.as_deref(), Some("hot"));
        let plan = MigrationPlan {
            source_pool: "hot".into(),
            target_pool: "cold".into(),
            stripe_count: u16::try_from(original.count).unwrap(),
            stripe_size: u32::try_from(original.size).unwrap(),
        };
        let expected = FileVersion::from(&fs::metadata(&file).unwrap());
        let deadline = Instant::now() + Duration::from_secs(15);
        let admitted = scoped_to(&mount, &team, &archive)
            .admit(&mount, &fid, deadline)
            .unwrap();
        let mut checks = 0;
        let mut remaining_link = link.clone();
        let result = LustreApi.migrate_reserved_pool(
            &mount,
            &fid,
            &plan,
            expected,
            MigrationLimits {
                deadline,
                expected_access: None,
                check_links: |source: &File| {
                    checks += 1;
                    if checks == 2 {
                        match scenario {
                            "outside" => fs::hard_link(&file, root.join("outside")).unwrap(),
                            "excluded" => {
                                remaining_link = archive.join("link");
                                fs::rename(&link, &remaining_link).unwrap();
                            }
                            "rename" => {
                                let renamed = team.join("renamed");
                                remaining_link = renamed.join("link");
                                fs::rename(&project, renamed).unwrap();
                            }
                            "incomplete" => fs::remove_file(&alias).unwrap(),
                            _ => unreachable!(),
                        }
                    }
                    admitted.recheck(&alias, &fid, source, deadline)
                },
                before_swap: || panic!("the operation must stop before the swap permit"),
            },
            MigrationOwnership::acquire(&filesystem).unwrap(),
        );
        let error = result.err();
        assert!(
            matches!(error, Some(MigrationError::Deferred(actual)) if actual == reason),
            "{scenario}: {error:?}"
        );
        assert_eq!(checks, 2);
        assert_eq!(LustreApi.path_to_fid(&remaining_link).unwrap(), fid);
        assert_eq!(LustreApi.path_to_fid(&file).unwrap(), fid);
        assert_eq!(LustreApi.migration_source_layout(&mount, &fid).unwrap(), original);
        assert_eq!(fs::read(&remaining_link).unwrap(), bytes);
        assert_eq!(
            fs::metadata(&file).unwrap().nlink(),
            if scenario == "outside" { 3 } else { 2 }
        );
    }
}
