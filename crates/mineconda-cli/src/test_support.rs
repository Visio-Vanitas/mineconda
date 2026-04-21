use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use mineconda_core::{
    DEFAULT_GROUP_NAME, LoaderKind, LoaderSpec, LockMetadata, LockedDependency,
    LockedDependencyKind, LockedPackage, Lockfile, Manifest, ModSide, ModSource, ModSpec,
    ProjectSection, RuntimeProfile, ServerProfile, WorkspaceConfig, lockfile_path, manifest_path,
    workspace_path, write_lockfile, write_manifest, write_workspace,
};
use mineconda_resolver::{ResolveRequest, resolve_lockfile};

pub(crate) fn test_manifest(mods: Vec<ModSpec>) -> Manifest {
    Manifest {
        project: ProjectSection {
            name: "test-pack".to_string(),
            minecraft: "1.21.1".to_string(),
            loader: LoaderSpec {
                kind: LoaderKind::NeoForge,
                version: "latest".to_string(),
            },
        },
        mods,
        groups: Default::default(),
        profiles: Default::default(),
        sources: Default::default(),
        cache: Default::default(),
        server: ServerProfile::default(),
        runtime: RuntimeProfile::default(),
    }
}

pub(crate) fn test_lockfile(packages: Vec<LockedPackage>) -> Lockfile {
    Lockfile {
        metadata: LockMetadata {
            generated_by: "test".to_string(),
            generated_at_unix: 0,
            minecraft: "1.21.1".to_string(),
            loader: LoaderSpec {
                kind: LoaderKind::NeoForge,
                version: "latest".to_string(),
            },
            dependency_graph: true,
            group_metadata: true,
        },
        packages,
    }
}

pub(crate) fn required_dependency(id: &str) -> LockedDependency {
    LockedDependency {
        source: ModSource::Modrinth,
        id: id.to_string(),
        kind: LockedDependencyKind::Required,
        constraint: None,
    }
}

pub(crate) fn incompatible_dependency(id: &str) -> LockedDependency {
    LockedDependency {
        source: ModSource::Modrinth,
        id: id.to_string(),
        kind: LockedDependencyKind::Incompatible,
        constraint: None,
    }
}

pub(crate) fn test_package(
    id: &str,
    version: &str,
    dependencies: Vec<LockedDependency>,
) -> LockedPackage {
    LockedPackage {
        id: id.to_string(),
        source: ModSource::Modrinth,
        version: version.to_string(),
        side: ModSide::Both,
        file_name: format!("{id}.jar"),
        install_path: None,
        file_size: Some(1),
        sha256: "deadbeef".to_string(),
        download_url: format!("https://example.invalid/{id}.jar"),
        hashes: Vec::new(),
        source_ref: Some(format!(
            "requested={id};project={id};version={version};name={id}"
        )),
        groups: vec![DEFAULT_GROUP_NAME.to_string()],
        dependencies,
    }
}

pub(crate) struct TempProject {
    pub(crate) path: PathBuf,
}

impl TempProject {
    pub(crate) fn new(name: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("mineconda-{name}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).expect("temp project directory");
        Self { path }
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(crate) fn write_workspace_fixture(root: &Path, members: &[&str]) {
    let workspace = WorkspaceConfig {
        workspace: mineconda_core::WorkspaceSection {
            name: "demo".to_string(),
            members: Vec::new(),
        },
        members: members.iter().map(|member| (*member).to_string()).collect(),
        profiles: Default::default(),
        runtime: None,
    };
    write_workspace(&workspace_path(root), &workspace).expect("write workspace");
    for member in members {
        fs::create_dir_all(root.join(member)).expect("member dir");
    }
}

pub(crate) fn write_local_member_manifest(root: &Path, id: &str) -> Manifest {
    fs::create_dir_all(root.join("vendor")).expect("vendor dir");
    fs::write(root.join(format!("vendor/{id}.jar")), id.as_bytes()).expect("fixture jar");
    let manifest = test_manifest(vec![ModSpec::new(
        id.to_string(),
        ModSource::Local,
        format!("vendor/{id}.jar"),
        ModSide::Both,
    )]);
    write_manifest(&manifest_path(root), &manifest).expect("write manifest");
    manifest
}

pub(crate) fn write_lock_for_manifest(root: &Path, manifest: &Manifest) -> Lockfile {
    let output = resolve_lockfile(
        manifest,
        None,
        &ResolveRequest {
            upgrade: false,
            groups: BTreeSet::from([DEFAULT_GROUP_NAME.to_string()]),
        },
    )
    .expect("resolve lock");
    write_lockfile(&lockfile_path(root), &output.lockfile).expect("write lock");
    output.lockfile
}

pub(crate) fn install_locked_packages(root: &Path, lock: &Lockfile) {
    fs::create_dir_all(root.join("mods")).expect("mods dir");
    for package in &lock.packages {
        fs::write(root.join(package.install_path_or_default()), b"installed")
            .expect("install package");
    }
}
