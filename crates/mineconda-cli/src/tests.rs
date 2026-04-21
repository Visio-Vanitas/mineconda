use std::collections::BTreeSet;
use std::fs;

use mineconda_core::{
    DEFAULT_GROUP_NAME, Manifest, lockfile_path, manifest_path, write_lockfile, write_manifest,
};
use mineconda_resolver::{ResolveRequest, resolve_lockfile};

use crate::command::lock::build_lock_diff_json_report;
use crate::command::status::build_status_json_report;
use crate::test_support::TempProject;

#[test]
fn status_and_lock_diff_ignore_inactive_group_entries() {
    let project = TempProject::new("status-inactive-groups");
    fs::create_dir_all(project.path.join("vendor")).expect("vendor dir");
    fs::write(project.path.join("vendor/demo.jar"), b"demo").expect("demo jar");
    fs::write(project.path.join("vendor/iris.jar"), b"iris").expect("iris jar");
    let manifest: Manifest = toml::from_str(
        r#"
[project]
name = "pack"
minecraft = "1.21.1"

[project.loader]
kind = "neo-forge"
version = "latest"

[[mods]]
id = "demo"
source = "local"
version = "vendor/demo.jar"
side = "both"

[groups.client]
mods = [
  { id = "iris", source = "local", version = "vendor/iris.jar", side = "client" }
]
"#,
    )
    .expect("manifest");
    write_manifest(&manifest_path(&project.path), &manifest).expect("write manifest");
    let output = resolve_lockfile(
        &manifest,
        None,
        &ResolveRequest {
            upgrade: false,
            groups: BTreeSet::from([DEFAULT_GROUP_NAME.to_string(), "client".to_string()]),
        },
    )
    .expect("resolve lock");
    write_lockfile(&lockfile_path(&project.path), &output.lockfile).expect("write lock");
    fs::create_dir_all(project.path.join("mods")).expect("mods dir");
    let default_package = output
        .lockfile
        .packages
        .iter()
        .find(|package| package.id == "demo")
        .expect("default package");
    fs::write(
        project.path.join(default_package.install_path_or_default()),
        b"installed",
    )
    .expect("installed package");

    let status =
        build_status_json_report(&project.path, Vec::new(), false, &[], None).expect("status");
    assert_eq!(status.summary.state, "clean");

    let diff = build_lock_diff_json_report(&project.path, false, Vec::new(), false, &[], None)
        .expect("lock diff report")
        .expect("lock diff should succeed");
    assert!(diff.entries.is_empty());
}
