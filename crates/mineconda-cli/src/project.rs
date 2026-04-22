use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use mineconda_core::{
    DEFAULT_GROUP_NAME, LockedPackage, Lockfile, Manifest, ModSpec, WORKSPACE_FILE,
    WorkspaceConfig, is_default_group_name, is_valid_group_name, is_valid_profile_name,
    lockfile_path, manifest_path, read_lockfile, read_manifest, read_workspace, workspace_path,
};

use crate::cli::ScopeArgs;
use crate::lock_package_matches_spec;

#[derive(Debug, Clone)]
pub(crate) struct ProjectTarget {
    pub(crate) root: PathBuf,
    pub(crate) workspace: Option<WorkspaceConfig>,
    pub(crate) member_name: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceMemberTarget {
    pub(crate) name: String,
    pub(crate) root: PathBuf,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ProjectSelection<'a> {
    pub(crate) groups: &'a [String],
    pub(crate) all_groups: bool,
    pub(crate) profiles: &'a [String],
    pub(crate) workspace: Option<&'a WorkspaceConfig>,
    pub(crate) member_name: Option<&'a str>,
}

impl<'a> ProjectSelection<'a> {
    pub(crate) fn active_groups(self, manifest: &Manifest) -> Result<BTreeSet<String>> {
        activation_groups_with_profiles(
            manifest,
            self.workspace,
            self.groups,
            self.all_groups,
            self.profiles,
        )
    }

    pub(crate) fn fallback_groups(self) -> Vec<String> {
        requested_groups_fallback(self.groups, self.all_groups)
    }

    pub(crate) fn normalized_profiles(self) -> Result<Vec<String>> {
        normalized_profile_names(self.profiles)
    }

    pub(crate) fn workspace_name(self) -> Option<String> {
        self.workspace.map(|item| item.workspace.name.clone())
    }
}

pub(crate) fn normalize_group_selector(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("group name must not be empty");
    }
    if is_default_group_name(trimmed) {
        return Ok(DEFAULT_GROUP_NAME.to_string());
    }
    if !is_valid_group_name(trimmed) {
        bail!("invalid group name `{trimmed}` (expected lowercase kebab-case)");
    }
    Ok(trimmed.to_string())
}

pub(crate) fn normalize_named_group(raw: &str) -> Result<String> {
    let group = normalize_group_selector(raw)?;
    if is_default_group_name(&group) {
        bail!("`default` is the built-in root group and cannot be created or removed");
    }
    Ok(group)
}

pub(crate) fn normalize_profile_name(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("profile name must not be empty");
    }
    if !is_valid_profile_name(trimmed) {
        bail!("invalid profile name `{trimmed}` (expected lowercase kebab-case)");
    }
    Ok(trimmed.to_string())
}

pub(crate) fn normalize_member_entry(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("member path must not be empty");
    }

    let path = Path::new(trimmed);
    if path.is_absolute() {
        bail!("workspace member path must be relative");
    }

    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => parts.push(value.to_string_lossy().to_string()),
            Component::ParentDir => {
                bail!("workspace member path must not escape the workspace root")
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("workspace member path must be relative")
            }
        }
    }

    if parts.is_empty() {
        bail!("member path must not be empty");
    }

    Ok(parts.join("/"))
}

pub(crate) fn profile_groups_for_selection(
    manifest: &Manifest,
    workspace: Option<&WorkspaceConfig>,
    profiles: &[String],
) -> Result<Vec<String>> {
    let mut groups = Vec::new();
    for raw in profiles {
        let profile = normalize_profile_name(raw)?;
        let spec = manifest
            .profile(&profile)
            .or_else(|| workspace.and_then(|item| item.profiles.0.get(&profile)))
            .with_context(|| format!("profile `{profile}` not found"))?;
        groups.extend(spec.groups.iter().cloned());
    }
    Ok(groups)
}

pub(crate) fn activation_groups_with_profiles(
    manifest: &Manifest,
    workspace: Option<&WorkspaceConfig>,
    requested: &[String],
    all_groups: bool,
    profiles: &[String],
) -> Result<BTreeSet<String>> {
    let mut combined = profile_groups_for_selection(manifest, workspace, profiles)?;
    combined.extend(requested.iter().cloned());
    activation_groups(manifest, &combined, all_groups)
}

pub(crate) fn validate_manifest_profiles(manifest: &Manifest) -> Result<()> {
    for (name, profile) in &manifest.profiles.0 {
        if !is_valid_profile_name(name) {
            bail!("manifest contains invalid profile name `{name}`");
        }
        for group in &profile.groups {
            normalize_group_selector(group).with_context(|| {
                format!("manifest profile `{name}` contains invalid group selector `{group}`")
            })?;
        }
    }
    Ok(())
}

pub(crate) fn validate_manifest_groups(manifest: &Manifest) -> Result<()> {
    for group in manifest.groups.0.keys() {
        if !is_valid_group_name(group) {
            bail!("manifest contains invalid group name `{group}`");
        }
    }
    validate_manifest_profiles(manifest)?;
    Ok(())
}

pub(crate) fn validate_workspace_config(workspace: &WorkspaceConfig) -> Result<()> {
    if workspace.workspace.name.trim().is_empty() {
        bail!("workspace name must not be empty");
    }
    for member in workspace.member_entries() {
        normalize_member_entry(member)
            .with_context(|| format!("workspace contains invalid member `{member}`"))?;
    }
    for (name, profile) in &workspace.profiles.0 {
        if !is_valid_profile_name(name) {
            bail!("workspace contains invalid profile name `{name}`");
        }
        for group in &profile.groups {
            normalize_group_selector(group).with_context(|| {
                format!("workspace profile `{name}` contains invalid group selector `{group}`")
            })?;
        }
    }
    Ok(())
}

pub(crate) fn load_workspace_optional(root: &Path) -> Result<Option<WorkspaceConfig>> {
    let path = workspace_path(root);
    if !path.exists() {
        return Ok(None);
    }
    let mut workspace =
        read_workspace(&path).with_context(|| format!("failed to read {}", path.display()))?;
    if workspace.members.is_empty() && !workspace.workspace.members.is_empty() {
        workspace.members = workspace.workspace.members.clone();
        workspace.workspace.members.clear();
    }
    validate_workspace_config(&workspace)?;
    Ok(Some(workspace))
}

pub(crate) fn load_workspace_required(root: &Path) -> Result<WorkspaceConfig> {
    load_workspace_optional(root)?.with_context(|| {
        format!(
            "workspace not found, expected {}",
            workspace_path(root).display()
        )
    })
}

pub(crate) fn load_manifest(root: &Path) -> Result<Manifest> {
    let path = manifest_path(root);
    let manifest =
        read_manifest(&path).with_context(|| format!("failed to read {}", path.display()))?;
    validate_manifest_groups(&manifest)?;
    Ok(manifest)
}

pub(crate) fn load_manifest_optional(root: &Path) -> Result<Option<Manifest>> {
    let path = manifest_path(root);
    if !path.exists() {
        return Ok(None);
    }
    let manifest =
        read_manifest(&path).with_context(|| format!("failed to read {}", path.display()))?;
    validate_manifest_groups(&manifest)?;
    Ok(Some(manifest))
}

pub(crate) fn load_lockfile_optional(root: &Path) -> Result<Option<Lockfile>> {
    let path = lockfile_path(root);
    if !path.exists() {
        return Ok(None);
    }
    let lock =
        read_lockfile(&path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(Some(lock))
}

pub(crate) fn load_lockfile_required(root: &Path) -> Result<Lockfile> {
    load_lockfile_optional(root)?.context("lockfile not found, run `mineconda lock` first")
}

pub(crate) fn workspace_member_target(
    workspace_root: &Path,
    workspace: &WorkspaceConfig,
    selector: &str,
) -> Result<WorkspaceMemberTarget> {
    let exact = normalize_member_entry(selector).ok();
    if let Some(exact) = exact
        && workspace
            .member_entries()
            .iter()
            .any(|member| member == &exact)
    {
        return Ok(WorkspaceMemberTarget {
            name: exact.clone(),
            root: workspace_root.join(&exact),
        });
    }

    let selector = selector.trim();
    let mut matches = workspace
        .member_entries()
        .iter()
        .filter(|member| {
            Path::new(member)
                .file_name()
                .map(|value| value == selector)
                .unwrap_or(false)
        })
        .cloned()
        .collect::<Vec<_>>();
    matches.sort();
    matches.dedup();

    match matches.as_slice() {
        [member] => Ok(WorkspaceMemberTarget {
            name: member.clone(),
            root: workspace_root.join(member),
        }),
        [] => bail!("workspace member `{selector}` not found"),
        _ => bail!(
            "workspace member `{selector}` is ambiguous; use one of: {}",
            matches.join(", ")
        ),
    }
}

pub(crate) fn workspace_members(
    workspace_root: &Path,
    workspace: &WorkspaceConfig,
) -> Result<Vec<WorkspaceMemberTarget>> {
    workspace
        .member_entries()
        .iter()
        .map(|member| {
            Ok(WorkspaceMemberTarget {
                name: normalize_member_entry(member)?,
                root: workspace_root.join(member),
            })
        })
        .collect()
}

pub(crate) fn resolve_project_target(root: &Path, scope: &ScopeArgs) -> Result<ProjectTarget> {
    let workspace = load_workspace_optional(root)?;
    match workspace {
        Some(workspace) => {
            let member = scope.member.as_deref().with_context(|| {
                "workspace root requires --member for this command; use `--all-members` where supported"
            })?;
            let target = workspace_member_target(root, &workspace, member)?;
            Ok(ProjectTarget {
                root: target.root,
                workspace: Some(workspace),
                member_name: Some(target.name),
            })
        }
        None => {
            if scope.workspace {
                bail!("{} does not contain {}", root.display(), WORKSPACE_FILE);
            }
            if scope.member.is_some() || scope.all_members {
                bail!("--member/--all-members require a workspace root");
            }
            Ok(ProjectTarget {
                root: root.to_path_buf(),
                workspace: None,
                member_name: None,
            })
        }
    }
}

pub(crate) fn target_group_name(group: Option<String>) -> Result<String> {
    match group {
        Some(group) => normalize_group_selector(&group),
        None => Ok(DEFAULT_GROUP_NAME.to_string()),
    }
}

pub(crate) fn activation_groups(
    manifest: &Manifest,
    requested: &[String],
    all_groups: bool,
) -> Result<BTreeSet<String>> {
    let mut groups = BTreeSet::from([DEFAULT_GROUP_NAME.to_string()]);

    if all_groups {
        groups.extend(
            manifest
                .group_names()
                .into_iter()
                .filter(|group| !is_default_group_name(group)),
        );
        return Ok(groups);
    }

    for raw in requested {
        let group = normalize_group_selector(raw)?;
        if is_default_group_name(&group) {
            continue;
        }
        if !manifest.has_named_group(&group) {
            bail!("group `{group}` not found in manifest");
        }
        groups.insert(group);
    }

    Ok(groups)
}

pub(crate) fn edit_groups(
    manifest: &Manifest,
    requested: &[String],
    all_groups: bool,
) -> Result<Vec<String>> {
    if all_groups {
        return Ok(manifest.group_names());
    }

    if requested.is_empty() {
        return Ok(vec![DEFAULT_GROUP_NAME.to_string()]);
    }

    let mut groups = BTreeSet::new();
    for raw in requested {
        let group = normalize_group_selector(raw)?;
        if !is_default_group_name(&group) && !manifest.has_named_group(&group) {
            bail!("group `{group}` not found in manifest");
        }
        groups.insert(group);
    }

    Ok(groups.into_iter().collect())
}

pub(crate) fn package_in_groups(package: &LockedPackage, groups: &BTreeSet<String>) -> bool {
    if package.groups.is_empty() {
        return groups.contains(DEFAULT_GROUP_NAME);
    }
    package.groups.iter().any(|group| groups.contains(group))
}

pub(crate) fn selected_manifest_specs<'a>(
    manifest: &'a Manifest,
    groups: &BTreeSet<String>,
) -> Vec<(String, &'a ModSpec)> {
    let mut entries = Vec::new();
    for group in groups {
        if let Some(mods) = manifest.group_mods(group) {
            entries.extend(mods.iter().map(|spec| (group.clone(), spec)));
        }
    }
    entries
}

pub(crate) fn ensure_lock_group_metadata(
    manifest: &Manifest,
    lock: &Lockfile,
    groups: &BTreeSet<String>,
) -> Result<()> {
    let needs_group_metadata = !manifest.groups.is_empty() || groups.len() > 1;
    if needs_group_metadata && !lock.metadata.group_metadata {
        bail!("lockfile does not contain dependency group data; rerun `mineconda lock` first");
    }
    Ok(())
}

pub(crate) fn ensure_lock_covers_groups(
    manifest: &Manifest,
    lock: &Lockfile,
    groups: &BTreeSet<String>,
) -> Result<()> {
    ensure_lock_group_metadata(manifest, lock, groups)?;

    for (group, spec) in selected_manifest_specs(manifest, groups) {
        let matches = lock.packages.iter().any(|package| {
            package_in_groups(package, &BTreeSet::from([group.clone()]))
                && lock_package_matches_spec(package, spec)
        });
        if !matches {
            bail!(
                "lockfile does not contain group `{group}` entry `{}` [{}]; rerun `mineconda lock` with the required groups",
                spec.id,
                spec.source.as_str()
            );
        }
    }

    Ok(())
}

pub(crate) fn filtered_lockfile(lock: &Lockfile, groups: &BTreeSet<String>) -> Lockfile {
    let mut filtered = lock.clone();
    filtered
        .packages
        .retain(|package| package_in_groups(package, groups));
    filtered
}

pub(crate) fn filtered_manifest_for_export(
    manifest: &Manifest,
    groups: &BTreeSet<String>,
) -> Manifest {
    let mut filtered = manifest.clone();
    filtered.mods = selected_manifest_specs(manifest, groups)
        .into_iter()
        .map(|(_, spec)| spec.clone())
        .collect();
    filtered.groups = Default::default();
    filtered
}

pub(crate) fn format_group_list(groups: &[String]) -> String {
    if groups.is_empty() {
        return DEFAULT_GROUP_NAME.to_string();
    }
    groups.join(",")
}

pub(crate) fn format_active_groups(groups: &BTreeSet<String>) -> String {
    format_group_list(&groups.iter().cloned().collect::<Vec<_>>())
}

pub(crate) fn requested_groups_fallback(groups: &[String], all_groups: bool) -> Vec<String> {
    if all_groups {
        return vec![DEFAULT_GROUP_NAME.to_string()];
    }

    let mut out = BTreeSet::from([DEFAULT_GROUP_NAME.to_string()]);
    for raw in groups {
        if let Ok(group) = normalize_group_selector(raw) {
            out.insert(group);
        }
    }
    out.into_iter().collect()
}

pub(crate) fn format_selection_args(
    groups: &[String],
    all_groups: bool,
    profiles: &[String],
) -> Result<String> {
    let mut args = Vec::new();

    if !all_groups {
        for profile in normalized_profile_names(profiles)? {
            args.push(format!("--profile {profile}"));
        }
    }

    if all_groups {
        args.push("--all-groups".to_string());
    } else {
        let mut seen = BTreeSet::new();
        for raw in groups {
            let group = normalize_group_selector(raw)?;
            if !is_default_group_name(&group) && seen.insert(group.clone()) {
                args.push(format!("--group {group}"));
            }
        }
    }

    if args.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!(" {}", args.join(" ")))
    }
}

pub(crate) fn format_selection_command(
    command: &str,
    groups: &[String],
    all_groups: bool,
    profiles: &[String],
) -> Result<String> {
    Ok(format!(
        "{command}{}",
        format_selection_args(groups, all_groups, profiles)?
    ))
}

pub(crate) fn normalized_profile_names(profiles: &[String]) -> Result<Vec<String>> {
    profiles
        .iter()
        .map(|profile| normalize_profile_name(profile))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use mineconda_core::{
        DEFAULT_GROUP_NAME, Manifest, WorkspaceConfig, workspace_path, write_workspace,
    };

    use super::*;
    use crate::cli::ScopeArgs;
    use crate::test_support::TempProject;

    fn test_manifest_with_client_group() -> Manifest {
        toml::from_str(
            r#"
[project]
name = "pack"
minecraft = "1.21.1"

[project.loader]
kind = "neo-forge"
version = "latest"

[[mods]]
id = "jei"
source = "modrinth"
version = "latest"
side = "both"

[groups.client]
mods = [
  { id = "iris", source = "modrinth", version = "latest", side = "client" }
]
"#,
        )
        .expect("manifest should parse")
    }

    #[test]
    fn activation_groups_include_default_plus_requested_extras() {
        let manifest = test_manifest_with_client_group();
        let groups =
            activation_groups(&manifest, &["client".to_string()], false).expect("active groups");
        assert_eq!(
            groups,
            BTreeSet::from([DEFAULT_GROUP_NAME.to_string(), "client".to_string()])
        );
    }

    #[test]
    fn activation_groups_include_workspace_profile_groups() {
        let manifest = test_manifest_with_client_group();
        let workspace: WorkspaceConfig = toml::from_str(
            r#"
[workspace]
name = "demo"

members = ["packs/client"]

[profiles.client-dev]
groups = ["client"]
"#,
        )
        .expect("workspace config");

        let groups = activation_groups_with_profiles(
            &manifest,
            Some(&workspace),
            &[],
            false,
            &["client-dev".to_string()],
        )
        .expect("profile groups");
        assert_eq!(
            groups,
            BTreeSet::from([DEFAULT_GROUP_NAME.to_string(), "client".to_string()])
        );
    }

    #[test]
    fn project_profile_overrides_workspace_profile() {
        let manifest: Manifest = toml::from_str(
            r#"
[project]
name = "pack"
minecraft = "1.21.1"

[project.loader]
kind = "neo-forge"
version = "latest"

[groups.client]
mods = []

[groups.dev]
mods = []

[profiles.client-dev]
groups = ["client"]
"#,
        )
        .expect("manifest config");
        let workspace: WorkspaceConfig = toml::from_str(
            r#"
[workspace]
name = "demo"

members = ["packs/client"]

[profiles.client-dev]
groups = ["client", "dev"]
"#,
        )
        .expect("workspace config");

        let groups = activation_groups_with_profiles(
            &manifest,
            Some(&workspace),
            &[],
            false,
            &["client-dev".to_string()],
        )
        .expect("profile groups");
        assert_eq!(
            groups,
            BTreeSet::from([DEFAULT_GROUP_NAME.to_string(), "client".to_string()])
        );
    }

    #[test]
    fn edit_groups_target_only_requested_group() {
        let manifest = test_manifest_with_client_group();
        let groups = edit_groups(&manifest, &["client".to_string()], false).expect("edit groups");
        assert_eq!(groups, vec!["client".to_string()]);
    }

    #[test]
    fn resolve_project_target_finds_workspace_member_by_basename() {
        let project = TempProject::new("workspace-target");
        let workspace = WorkspaceConfig {
            workspace: mineconda_core::WorkspaceSection {
                name: "demo".to_string(),
                members: Vec::new(),
            },
            members: vec!["packs/client".to_string()],
            profiles: Default::default(),
            runtime: None,
        };
        write_workspace(&workspace_path(&project.path), &workspace).expect("write workspace");
        fs::create_dir_all(project.path.join("packs/client")).expect("member dir");

        let target = resolve_project_target(
            &project.path,
            &ScopeArgs {
                workspace: false,
                member: Some("client".to_string()),
                all_members: false,
                profiles: Vec::new(),
            },
        )
        .expect("resolve target");

        assert_eq!(target.root, project.path.join("packs/client"));
        assert_eq!(target.member_name.as_deref(), Some("packs/client"));
        assert!(target.workspace.is_some());
    }
}
