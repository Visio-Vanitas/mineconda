use crate::command::lock::write_lock_from_manifest;
use crate::*;
pub(crate) fn cmd_init(
    root: &Path,
    name: String,
    minecraft: String,
    loader: LoaderArg,
    loader_version: String,
    bare: bool,
) -> Result<()> {
    fs::create_dir_all(root)
        .with_context(|| format!("failed to create root {}", root.display()))?;
    let path = manifest_path(root);
    if path.exists() {
        bail!("manifest already exists at {}", path.display());
    }

    let manifest = Manifest::new(name, minecraft, loader.to_core(), loader_version);
    write_manifest(&path, &manifest)
        .with_context(|| format!("failed to write {}", path.display()))?;
    if !bare {
        init_modpack_layout(root)?;
    }
    println!("initialized {}", path.display());
    Ok(())
}

pub(crate) fn cmd_add(
    root: &Path,
    id: String,
    source: SourceArg,
    version: String,
    side: SideArg,
    group: Option<String>,
    no_lock: bool,
) -> Result<()> {
    let path = manifest_path(root);
    let mut manifest = load_manifest(root)?;
    let group = target_group_name(group)?;

    let source = source.to_core();
    if source == ModSource::S3
        && (version.eq_ignore_ascii_case("latest")
            || version.starts_with('^')
            || version.starts_with('~')
            || version.contains(',')
            || version.contains('>')
            || version.contains('<')
            || version.contains('*')
            || version.contains('|')
            || version.contains('='))
    {
        bail!(
            "s3 source requires exact object key (or s3://bucket/key), use --version to provide one"
        );
    }
    let side = side.to_core();
    let group_mods = manifest.ensure_group_mods_mut(&group);
    if let Some(existing) = group_mods
        .iter_mut()
        .find(|entry| entry.id == id && entry.source == source)
    {
        existing.version = version.clone();
        existing.side = side;
        println!("updated mod {} in {} [{}]", id, path.display(), group);
    } else {
        group_mods.push(ModSpec::new(id.clone(), source, version, side));
        println!("added mod {} to {} [{}]", id, path.display(), group);
    }

    write_manifest(&path, &manifest)?;
    if !no_lock {
        let groups = if is_default_group_name(&group) {
            BTreeSet::new()
        } else {
            BTreeSet::from([group.clone()])
        };
        write_lock_from_manifest(root, &manifest, false, groups)?;
    }
    Ok(())
}

pub(crate) fn cmd_remove(
    root: &Path,
    id: &str,
    source: Option<SourceArg>,
    group: Option<String>,
    all_groups: bool,
    no_lock: bool,
) -> Result<()> {
    let path = manifest_path(root);
    let mut manifest = load_manifest(root)?;
    let source_filter = source.map(SourceArg::to_core);
    let target_groups = if all_groups {
        manifest.group_names()
    } else {
        vec![target_group_name(group)?]
    };
    let mut removed = 0usize;

    for group in &target_groups {
        let Some(mods) = manifest.group_mods_mut(group) else {
            continue;
        };
        let before = mods.len();
        mods.retain(|item| {
            if item.id != id {
                return true;
            }
            match source_filter {
                Some(source) => item.source != source,
                None => false,
            }
        });
        removed += before.saturating_sub(mods.len());
    }
    write_manifest(&path, &manifest)?;
    println!("removed {removed} matching entries");
    if !no_lock {
        let groups = if all_groups {
            activation_groups(&manifest, &[], true)?
        } else {
            let selected = target_group_name(target_groups.first().cloned())?;
            if is_default_group_name(&selected) || !manifest.has_named_group(&selected) {
                BTreeSet::new()
            } else {
                BTreeSet::from([selected])
            }
        };
        write_lock_from_manifest(root, &manifest, false, groups)?;
    }
    Ok(())
}

pub(crate) fn cmd_group(root: &Path, command: GroupCommands) -> Result<()> {
    match command {
        GroupCommands::Ls => cmd_group_ls(root),
        GroupCommands::Add { name } => cmd_group_add(root, &name),
        GroupCommands::Remove { name, no_lock } => cmd_group_remove(root, &name, no_lock),
    }
}

fn cmd_group_ls(root: &Path) -> Result<()> {
    let manifest = load_manifest(root)?;
    for group in manifest.group_names() {
        let count = manifest
            .group_mods(&group)
            .map(|mods| mods.len())
            .unwrap_or(0);
        println!("{group}\t{count}");
    }
    Ok(())
}

fn cmd_group_add(root: &Path, name: &str) -> Result<()> {
    let path = manifest_path(root);
    let mut manifest = load_manifest(root)?;
    let group = normalize_named_group(name)?;
    if manifest.has_named_group(&group) {
        bail!("group `{group}` already exists");
    }
    manifest.ensure_group_mods_mut(&group);
    write_manifest(&path, &manifest)?;
    println!("added group `{group}` to {}", path.display());
    Ok(())
}

fn cmd_group_remove(root: &Path, name: &str, no_lock: bool) -> Result<()> {
    let path = manifest_path(root);
    let mut manifest = load_manifest(root)?;
    let group = normalize_named_group(name)?;
    let Some(removed) = manifest.remove_named_group(&group) else {
        bail!("group `{group}` not found");
    };
    write_manifest(&path, &manifest)?;
    println!(
        "removed group `{group}` (mods={}) from {}",
        removed.mods.len(),
        path.display()
    );
    if !no_lock {
        write_lock_from_manifest(root, &manifest, false, BTreeSet::new())?;
    }
    Ok(())
}

pub(crate) fn cmd_profile(root: &Path, command: ProfileCommands, scope: &ScopeArgs) -> Result<()> {
    let workspace = load_workspace_optional(root)?;
    let use_workspace_scope = scope.workspace || (workspace.is_some() && scope.member.is_none());

    if use_workspace_scope {
        let path = workspace_path(root);
        let mut workspace = workspace
            .with_context(|| format!("workspace not found, expected {}", path.display()))?;
        match command {
            ProfileCommands::Ls => {
                if workspace.profiles.0.is_empty() {
                    println!("no workspace profiles defined");
                    return Ok(());
                }
                for (name, profile) in &workspace.profiles.0 {
                    println!("{name}\t{}\tworkspace", format_group_list(&profile.groups));
                }
            }
            ProfileCommands::Add { name, groups } => {
                let name = normalize_profile_name(&name)?;
                let normalized = groups
                    .iter()
                    .map(|group| normalize_group_selector(group))
                    .collect::<Result<Vec<_>>>()?;
                workspace.profiles.0.insert(
                    name.clone(),
                    mineconda_core::GroupProfile { groups: normalized },
                );
                write_workspace(&path, &workspace)?;
                println!("updated workspace profile `{name}` in {}", path.display());
            }
            ProfileCommands::Remove { name } => {
                let name = normalize_profile_name(&name)?;
                if workspace.profiles.0.remove(&name).is_none() {
                    bail!("profile `{name}` not found");
                }
                write_workspace(&path, &workspace)?;
                println!("removed workspace profile `{name}` from {}", path.display());
            }
        }
        return Ok(());
    }

    let target = resolve_project_target(root, scope)?;
    let path = manifest_path(&target.root);
    let mut manifest = load_manifest(&target.root)?;
    match command {
        ProfileCommands::Ls => {
            let mut merged: HashMap<String, (Vec<String>, &'static str)> = HashMap::new();
            if let Some(workspace) = target.workspace.as_ref() {
                for (name, profile) in &workspace.profiles.0 {
                    merged.insert(name.clone(), (profile.groups.clone(), "workspace"));
                }
            }
            for (name, profile) in &manifest.profiles.0 {
                merged.insert(name.clone(), (profile.groups.clone(), "project"));
            }
            if merged.is_empty() {
                println!("no profiles defined");
                return Ok(());
            }
            let mut names = merged.into_iter().collect::<Vec<_>>();
            names.sort_by(|left, right| left.0.cmp(&right.0));
            for (name, (groups, origin)) in names {
                println!("{name}\t{}\t{origin}", format_group_list(&groups));
            }
        }
        ProfileCommands::Add { name, groups } => {
            let name = normalize_profile_name(&name)?;
            let normalized = groups
                .iter()
                .map(|group| normalize_group_selector(group))
                .collect::<Result<Vec<_>>>()?;
            manifest.profiles.0.insert(
                name.clone(),
                mineconda_core::GroupProfile { groups: normalized },
            );
            write_manifest(&path, &manifest)?;
            println!("updated profile `{name}` in {}", path.display());
        }
        ProfileCommands::Remove { name } => {
            let name = normalize_profile_name(&name)?;
            if manifest.remove_profile(&name).is_none() {
                bail!("profile `{name}` not found");
            }
            write_manifest(&path, &manifest)?;
            println!("removed profile `{name}` from {}", path.display());
        }
    }
    Ok(())
}

pub(crate) fn cmd_workspace(root: &Path, command: WorkspaceCommands) -> Result<()> {
    match command {
        WorkspaceCommands::Init { name } => {
            fs::create_dir_all(root)
                .with_context(|| format!("failed to create workspace root {}", root.display()))?;
            let path = workspace_path(root);
            if path.exists() {
                bail!("workspace already exists at {}", path.display());
            }
            if manifest_path(root).exists() {
                bail!(
                    "workspace root already contains {}; keep workspace config separate from project manifests",
                    manifest_path(root).display()
                );
            }
            let workspace = WorkspaceConfig::new(name);
            write_workspace(&path, &workspace)?;
            println!("initialized {}", path.display());
        }
        WorkspaceCommands::Ls => {
            let workspace = load_workspace_required(root)?;
            println!(
                "workspace {}\tmembers={}",
                workspace.workspace.name,
                workspace.member_entries().len()
            );
            for member in workspace_members(root, &workspace)? {
                println!(
                    "{}\tmanifest={}\tlock={}",
                    member.name,
                    manifest_path(&member.root).exists(),
                    lockfile_path(&member.root).exists()
                );
            }
        }
        WorkspaceCommands::Add { path: member_path } => {
            let path = workspace_path(root);
            let mut workspace = load_workspace_required(root)?;
            let member = normalize_member_entry(&member_path)?;
            if workspace
                .member_entries()
                .iter()
                .any(|existing| existing == &member)
            {
                bail!("workspace member `{member}` already exists");
            }
            fs::create_dir_all(root.join(&member)).with_context(|| {
                format!(
                    "failed to create workspace member directory {}",
                    root.join(&member).display()
                )
            })?;
            workspace.members.push(member.clone());
            workspace.members.sort();
            workspace.members.dedup();
            write_workspace(&path, &workspace)?;
            println!("added workspace member `{member}` to {}", path.display());
        }
        WorkspaceCommands::Remove { path: member_path } => {
            let path = workspace_path(root);
            let mut workspace = load_workspace_required(root)?;
            let target = workspace_member_target(root, &workspace, &member_path)?;
            let before = workspace.member_entries().len();
            workspace.members.retain(|member| member != &target.name);
            if workspace.member_entries().len() == before {
                bail!("workspace member `{}` not found", target.name);
            }
            write_workspace(&path, &workspace)?;
            println!(
                "removed workspace member `{}` from {}",
                target.name,
                path.display()
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModListStatus {
    Synced,
    NotSynced,
    MissingInLock,
    Unlocked,
}

impl ModListStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Synced => "synced",
            Self::NotSynced => "not-synced",
            Self::MissingInLock => "unresolved",
            Self::Unlocked => "unlocked",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Self::Synced => "1;32",
            Self::NotSynced => "1;33",
            Self::MissingInLock => "1;31",
            Self::Unlocked => "1;36",
        }
    }
}

pub(crate) fn cmd_ls(
    root: &Path,
    show_status: bool,
    show_info: bool,
    no_color: bool,
    json: bool,
    selection: ProjectSelection<'_>,
) -> Result<()> {
    let manifest = load_manifest(root)?;
    let active_groups = selection.active_groups(&manifest)?;
    let selected_specs = selected_manifest_specs(&manifest, &active_groups);
    if selected_specs.is_empty() {
        if json {
            emit_json_report(
                &LsJsonReport {
                    command: "ls",
                    groups: active_groups.iter().cloned().collect(),
                    profiles: selection.normalized_profiles()?,
                    workspace: selection.workspace_name(),
                    member: selection.member_name.map(ToString::to_string),
                    summary: LsJsonSummary { roots: 0 },
                    items: Vec::new(),
                    messages: vec!["selected groups have no mods".to_string()],
                },
                0,
            )?;
            return Ok(());
        }
        println!("selected groups have no mods");
        return Ok(());
    }

    let lock = load_lockfile_optional(root)?;
    let has_lock = lock.is_some();
    let use_color =
        std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() && !no_color;
    let cache_root = if show_info {
        Some(cache_root_path()?)
    } else {
        None
    };
    let normalized_profiles = selection.normalized_profiles()?;
    let mut json_items = Vec::new();

    if !json {
        println!("📦 mods: {}", selected_specs.len());
    }
    for (index, (group, spec)) in selected_specs.iter().enumerate() {
        let single_group = BTreeSet::from([group.clone()]);
        let locked = lock.as_ref().and_then(|item| {
            item.packages.iter().find(|pkg| {
                package_in_groups(pkg, &single_group) && lock_package_matches_spec(pkg, spec)
            })
        });

        let state = match locked {
            Some(pkg) => {
                if package_install_target_path(root, pkg).exists() {
                    ModListStatus::Synced
                } else {
                    ModListStatus::NotSynced
                }
            }
            None => {
                if has_lock {
                    ModListStatus::MissingInLock
                } else {
                    ModListStatus::Unlocked
                }
            }
        };

        json_items.push(LsJsonItem {
            group: group.clone(),
            id: spec.id.clone(),
            source: spec.source.as_str().to_string(),
            requested_version: spec.version.clone(),
            locked_version: locked.map(|pkg| pkg.version.clone()),
            status: state.label().to_string(),
            install_path: spec.install_path_or_default(),
            side: spec.side.as_str().to_string(),
        });

        if json {
            continue;
        }

        let resolved = locked.map(|pkg| pkg.version.as_str()).unwrap_or("-");
        let mut line = format!(
            "{:>2}. {} [{}] group={} req={} side={}",
            index + 1,
            spec.id,
            spec.source.as_str(),
            group,
            spec.version,
            spec.side.as_str()
        );

        if show_status {
            line.push_str(&format!(" status={}", state.label()));
        }
        if show_status || show_info {
            line.push_str(&format!(" lock={resolved}"));
        }

        let color = if show_status { state.color() } else { "0;37" };
        println!("{}", paint(&line, color, use_color));

        if !show_info {
            continue;
        }

        if let Some(pkg) = locked {
            let size = pkg
                .file_size
                .map(format_bytes)
                .unwrap_or_else(|| "-".to_string());
            let source_ref = pkg.source_ref.as_deref().unwrap_or("-");
            println!("    file: {}", pkg.file_name);
            println!("    install-path: {}", pkg.install_path_or_default());
            println!("    size: {size}");
            println!("    sha256: {}", pkg.sha256);
            println!("    source-ref: {source_ref}");
            println!("    download: {}", pkg.download_url);
            if let Some(root) = cache_root.as_ref() {
                let cache_path = cache_path_for_package_in(root, pkg);
                println!(
                    "    cache: {} ({})",
                    cache_path.display(),
                    if cache_path.exists() { "hit" } else { "miss" }
                );
            }
        } else {
            println!("    file: -");
            println!("    size: -");
            println!("    sha256: -");
            println!("    source-ref: -");
            println!("    download: -");
        }
    }

    if json {
        emit_json_report(
            &LsJsonReport {
                command: "ls",
                groups: active_groups.iter().cloned().collect(),
                profiles: normalized_profiles,
                workspace: selection.workspace_name(),
                member: selection.member_name.map(ToString::to_string),
                summary: LsJsonSummary {
                    roots: json_items.len(),
                },
                items: json_items,
                messages: vec![format!(
                    "selected groups={}",
                    format_active_groups(&active_groups)
                )],
            },
            0,
        )?;
    }

    Ok(())
}

pub(crate) fn cmd_update(
    root: &Path,
    id: Option<String>,
    source: Option<SourceArg>,
    to: Option<String>,
    groups: Vec<String>,
    all_groups: bool,
    no_lock: bool,
) -> Result<()> {
    let mut manifest = load_manifest(root)?;
    let path = manifest_path(root);

    if let Some(id) = id {
        let source_filter = source.map(SourceArg::to_core);
        let target = to.unwrap_or_else(|| "latest".to_string());
        let target_groups = edit_groups(&manifest, &groups, all_groups)?;
        let mut changed = 0usize;

        for group in &target_groups {
            let Some(mods) = manifest.group_mods_mut(group) else {
                continue;
            };
            for spec in mods {
                if spec.id != id {
                    continue;
                }
                if let Some(source) = source_filter
                    && spec.source != source
                {
                    continue;
                }
                spec.version = target.clone();
                changed += 1;
            }
        }

        if changed == 0 {
            bail!("mod `{id}` not found in manifest");
        }

        write_manifest(&path, &manifest)?;
        println!("updated {changed} entries of `{id}` to constraint `{target}`");
        if !no_lock {
            write_lock_from_manifest(
                root,
                &manifest,
                true,
                activation_groups(&manifest, &target_groups, false)?,
            )?;
        }
        return Ok(());
    }

    if (source.is_some() || to.is_some() || !groups.is_empty() || all_groups)
        && (source.is_some() || to.is_some())
    {
        bail!("`--source` and `--to` require an <id>");
    }

    if no_lock {
        println!("no operation: use `mineconda update <id>` or remove `--no-lock`");
        return Ok(());
    }

    write_lock_from_manifest(
        root,
        &manifest,
        true,
        activation_groups(&manifest, &groups, all_groups)?,
    )?;
    Ok(())
}

pub(crate) fn cmd_pin(
    root: &Path,
    id: String,
    source: Option<SourceArg>,
    version: Option<String>,
    groups: Vec<String>,
    all_groups: bool,
    no_lock: bool,
) -> Result<()> {
    let mut manifest = load_manifest(root)?;
    let path = manifest_path(root);
    let source_filter = source.map(SourceArg::to_core);
    let pin_version = if let Some(version) = version {
        version
    } else {
        let lock = load_lockfile_required(root)?;
        let mut matched = lock
            .packages
            .iter()
            .filter(|pkg| lock_package_matches_request(pkg, id.as_str(), source_filter));
        let first = matched
            .next()
            .with_context(|| format!("mod `{id}` not found in lockfile"))?;
        if source_filter.is_none() && matched.next().is_some() {
            bail!("multiple lockfile entries match `{id}`, use `--source` to disambiguate");
        }
        first.version.clone()
    };

    let target_groups = edit_groups(&manifest, &groups, all_groups)?;
    let mut changed = 0usize;
    for group in &target_groups {
        let Some(mods) = manifest.group_mods_mut(group) else {
            continue;
        };
        for spec in mods {
            if spec.id != id {
                continue;
            }
            if let Some(source) = source_filter
                && spec.source != source
            {
                continue;
            }
            spec.version = pin_version.clone();
            changed += 1;
        }
    }

    if changed == 0 {
        bail!("mod `{id}` not found in manifest");
    }

    write_manifest(&path, &manifest)?;
    println!("pinned {changed} entries of `{id}` to `{pin_version}`");
    if !no_lock {
        write_lock_from_manifest(
            root,
            &manifest,
            false,
            activation_groups(&manifest, &target_groups, false)?,
        )?;
    }
    Ok(())
}

pub(crate) fn lock_package_matches_spec(pkg: &LockedPackage, spec: &ModSpec) -> bool {
    lock_package_matches_request(pkg, spec.id.as_str(), Some(spec.source))
}

pub(crate) fn lock_package_matches_request(
    pkg: &LockedPackage,
    requested_id: &str,
    source_filter: Option<ModSource>,
) -> bool {
    if source_filter.is_some_and(|source| pkg.source != source) {
        return false;
    }
    if pkg.id == requested_id {
        return true;
    }

    match pkg.source {
        ModSource::Modrinth => {
            parse_source_ref_field(pkg.source_ref.as_deref(), "requested") == Some(requested_id)
                || parse_source_ref_field(pkg.source_ref.as_deref(), "project")
                    == Some(requested_id)
        }
        ModSource::Curseforge => {
            parse_source_ref_field(pkg.source_ref.as_deref(), "mod") == Some(requested_id)
        }
        _ => false,
    }
}

fn parse_source_ref_field<'a>(source_ref: Option<&'a str>, key: &str) -> Option<&'a str> {
    source_ref.and_then(|value| {
        value.split(';').find_map(|part| {
            let (part_key, part_value) = part.split_once('=')?;
            if part_key == key {
                Some(part_value)
            } else {
                None
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use mineconda_core::{DEFAULT_GROUP_NAME, LockedPackage, ModSide, ModSource};

    use super::*;

    #[test]
    fn lock_package_matches_modrinth_requested_slug() {
        let package = LockedPackage {
            id: "sk9rgfiA".to_string(),
            source: ModSource::Modrinth,
            version: "1.0.15+mc1.21.1".to_string(),
            side: ModSide::Both,
            file_name: "embeddium.jar".to_string(),
            install_path: None,
            file_size: Some(1),
            sha256: "deadbeef".to_string(),
            download_url: "https://example.invalid/embeddium.jar".to_string(),
            hashes: Vec::new(),
            source_ref: Some(
                "requested=embeddium;project=sk9rgfiA;version=J7b96IEd;name=Embeddium".to_string(),
            ),
            groups: vec![DEFAULT_GROUP_NAME.to_string()],
            dependencies: Vec::new(),
        };

        assert!(lock_package_matches_request(
            &package,
            "embeddium",
            Some(ModSource::Modrinth)
        ));
        assert!(lock_package_matches_request(
            &package,
            "sk9rgfiA",
            Some(ModSource::Modrinth)
        ));
        assert!(!lock_package_matches_request(
            &package,
            "sodium",
            Some(ModSource::Modrinth)
        ));
    }
}
