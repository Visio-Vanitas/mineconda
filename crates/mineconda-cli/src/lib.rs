use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::Parser;
use mineconda_core::{
    DEFAULT_GROUP_NAME, JavaProvider, LoaderKind, LockedDependency, LockedDependencyKind,
    LockedPackage, Lockfile, Manifest, ModSide, ModSource, ModSpec, RuntimeProfile, S3CacheAuth,
    S3CacheConfig, S3SourceConfig, ServerProfile, WorkspaceConfig, is_default_group_name,
    lockfile_path, manifest_path, workspace_path, write_lockfile, write_manifest, write_workspace,
};
use mineconda_export::{
    ExportRequest, ImportRequest, OverrideScope, detect_pack_format, export_pack,
    import_pack_with_format,
};
use mineconda_resolver::{
    InstallVersionsRequest, ResolveRequest, SearchRequest, SearchSource, list_install_versions,
    resolve_loader_version, resolve_lockfile, search_mods,
};
use mineconda_runner::{LoaderHint, RunMode, RunRequest, run_game_instance};
use mineconda_runtime::{
    ensure_java_runtime, find_java_runtime, list_java_runtimes, resolve_java_binary,
};
use mineconda_sync::{
    RemotePruneRequest, SyncRequest, cache_path_for_package_in, cache_root_path,
    collect_cache_stats, remote_prune_s3_cache, sync_lockfile, verify_cache_entries,
};
use reqwest::blocking::Client;
use terminal_size::{Width, terminal_size};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

mod cli;
mod command;
mod i18n;
mod project;
mod report;
mod search_tui;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
mod workspace_batch;

use crate::cli::*;
use crate::command::cache::cmd_cache;
use crate::command::doctor::cmd_doctor;
use crate::command::env::cmd_env;
use crate::command::import_export::{cmd_export, cmd_import, package_install_target_path};
use crate::command::lock::{
    build_lock_check_report, build_lock_diff_json_report, build_lock_write_report, cmd_lock,
    cmd_lock_diff, render_status_json_report,
};
use crate::command::mods::{
    cmd_add, cmd_group, cmd_init, cmd_ls, cmd_pin, cmd_profile, cmd_remove, cmd_update,
    cmd_workspace, lock_package_matches_request, lock_package_matches_spec,
};
use crate::command::run::cmd_run;
use crate::command::search::{
    SearchSpinner, cmd_search, format_bytes, format_supported_side, loader_label, optional_value,
    paint, truncate_visual, wrap_visual,
};
use crate::command::status::{build_status_json_report, cmd_status};
use crate::command::sync::{build_sync_report, cmd_sync};
use crate::command::tree_why::{cmd_tree, cmd_why, lock_graph_key, locked_package_graph_key};
use crate::project::*;
use crate::report::*;
use crate::workspace_batch::*;
fn normalized_package_groups(package: &LockedPackage) -> Vec<String> {
    let mut groups = if package.groups.is_empty() {
        vec![DEFAULT_GROUP_NAME.to_string()]
    } else {
        package.groups.clone()
    };
    groups.sort_by(|left, right| {
        match (
            is_default_group_name(left.as_str()),
            is_default_group_name(right.as_str()),
        ) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => left.cmp(right),
        }
    });
    groups.dedup();
    groups
}

fn normalized_package_dependencies(package: &LockedPackage) -> Vec<LockedDependency> {
    let mut dependencies = package.dependencies.clone();
    dependencies.sort_by(|left, right| {
        (
            left.id.as_str(),
            left.source.as_str(),
            left.kind.as_str(),
            left.constraint.as_deref().unwrap_or(""),
        )
            .cmp(&(
                right.id.as_str(),
                right.source.as_str(),
                right.kind.as_str(),
                right.constraint.as_deref().unwrap_or(""),
            ))
    });
    dependencies
}

fn package_artifact_signature(package: &LockedPackage) -> String {
    format!(
        "file={} install={} url={}",
        package.file_name,
        package.install_path_or_default(),
        package.download_url
    )
}

fn version_number_tokens(raw: &str) -> Vec<u64> {
    let mut values = Vec::new();
    let mut current = String::new();

    for ch in raw.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else if !current.is_empty() {
            if let Ok(value) = current.parse::<u64>() {
                values.push(value);
            }
            current.clear();
        }
    }

    values
}

fn compare_version_change(current: &str, desired: &str) -> LockDiffKind {
    let current_tokens = version_number_tokens(current);
    let desired_tokens = version_number_tokens(desired);

    if !current_tokens.is_empty() && !desired_tokens.is_empty() {
        match current_tokens.cmp(&desired_tokens) {
            std::cmp::Ordering::Less => return LockDiffKind::Upgrade,
            std::cmp::Ordering::Greater => return LockDiffKind::Downgrade,
            std::cmp::Ordering::Equal => {}
        }
    }

    LockDiffKind::ChangeVersion
}

fn compute_lock_diff_entries(current: Option<&Lockfile>, desired: &Lockfile) -> Vec<LockDiffEntry> {
    let mut entries = Vec::new();
    let current_by_key: HashMap<String, &LockedPackage> = current
        .map(|lock| {
            lock.packages
                .iter()
                .map(|package| (locked_package_graph_key(package), package))
                .collect()
        })
        .unwrap_or_default();
    let desired_by_key: HashMap<String, &LockedPackage> = desired
        .packages
        .iter()
        .map(|package| (locked_package_graph_key(package), package))
        .collect();

    for package in &desired.packages {
        let key = locked_package_graph_key(package);
        let desired_groups = normalized_package_groups(package);
        let desired_dependencies = normalized_package_dependencies(package);
        match current_by_key.get(&key) {
            None => entries.push(LockDiffEntry {
                kind: LockDiffKind::Add,
                id: package.id.clone(),
                source: package.source,
                current_version: None,
                desired_version: Some(package.version.clone()),
                current_groups: Vec::new(),
                desired_groups,
                current_dependencies: Vec::new(),
                desired_dependencies,
                current_artifact: None,
                desired_artifact: Some(package_artifact_signature(package)),
            }),
            Some(existing) => {
                let current_groups = normalized_package_groups(existing);
                let current_dependencies = normalized_package_dependencies(existing);
                if existing.version != package.version {
                    entries.push(LockDiffEntry {
                        kind: compare_version_change(&existing.version, &package.version),
                        id: package.id.clone(),
                        source: package.source,
                        current_version: Some(existing.version.clone()),
                        desired_version: Some(package.version.clone()),
                        current_groups: current_groups.clone(),
                        desired_groups: desired_groups.clone(),
                        current_dependencies: current_dependencies.clone(),
                        desired_dependencies: desired_dependencies.clone(),
                        current_artifact: Some(package_artifact_signature(existing)),
                        desired_artifact: Some(package_artifact_signature(package)),
                    });
                } else if package_artifact_signature(existing)
                    != package_artifact_signature(package)
                {
                    entries.push(LockDiffEntry {
                        kind: LockDiffKind::ChangeArtifact,
                        id: package.id.clone(),
                        source: package.source,
                        current_version: Some(existing.version.clone()),
                        desired_version: Some(package.version.clone()),
                        current_groups: current_groups.clone(),
                        desired_groups: desired_groups.clone(),
                        current_dependencies: current_dependencies.clone(),
                        desired_dependencies: desired_dependencies.clone(),
                        current_artifact: Some(package_artifact_signature(existing)),
                        desired_artifact: Some(package_artifact_signature(package)),
                    });
                }

                if current_groups != desired_groups {
                    entries.push(LockDiffEntry {
                        kind: LockDiffKind::ChangeGroups,
                        id: package.id.clone(),
                        source: package.source,
                        current_version: Some(existing.version.clone()),
                        desired_version: Some(package.version.clone()),
                        current_groups,
                        desired_groups,
                        current_dependencies: current_dependencies.clone(),
                        desired_dependencies: desired_dependencies.clone(),
                        current_artifact: None,
                        desired_artifact: None,
                    });
                }

                if current_dependencies != desired_dependencies {
                    entries.push(LockDiffEntry {
                        kind: LockDiffKind::ChangeDependencies,
                        id: package.id.clone(),
                        source: package.source,
                        current_version: Some(existing.version.clone()),
                        desired_version: Some(package.version.clone()),
                        current_groups: normalized_package_groups(existing),
                        desired_groups: normalized_package_groups(package),
                        current_dependencies,
                        desired_dependencies,
                        current_artifact: None,
                        desired_artifact: None,
                    });
                }
            }
        }
    }

    if let Some(current) = current {
        for package in &current.packages {
            let key = locked_package_graph_key(package);
            if desired_by_key.contains_key(&key) {
                continue;
            }
            entries.push(LockDiffEntry {
                kind: LockDiffKind::Remove,
                id: package.id.clone(),
                source: package.source,
                current_version: Some(package.version.clone()),
                desired_version: None,
                current_groups: normalized_package_groups(package),
                desired_groups: Vec::new(),
                current_dependencies: normalized_package_dependencies(package),
                desired_dependencies: Vec::new(),
                current_artifact: Some(package_artifact_signature(package)),
                desired_artifact: None,
            });
        }
    }

    entries.sort_by(|left, right| {
        (lock_graph_key(left.source, &left.id), left.kind)
            .cmp(&(lock_graph_key(right.source, &right.id), right.kind))
    });
    entries
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let root = cli.root;
    let scope = ScopeArgs {
        workspace: cli.workspace,
        member: cli.member,
        all_members: cli.all_members,
        profiles: cli.profiles,
    };
    let no_color = cli.no_color;
    i18n::init(cli.lang.to_preference());

    match cli.command {
        Commands::Workspace { command } => cmd_workspace(&root, command)?,
        Commands::Profile { command } => cmd_profile(&root, command, &scope)?,
        Commands::Init {
            name,
            minecraft,
            loader,
            loader_version,
            bare,
        } => {
            let target_root = if load_workspace_optional(&root)?.is_some() {
                let member = scope.member.as_deref().with_context(
                    || "workspace root requires --member to initialize a member project",
                )?;
                let workspace = load_workspace_required(&root)?;
                workspace_member_target(&root, &workspace, member)?.root
            } else {
                root.clone()
            };
            cmd_init(&target_root, name, minecraft, loader, loader_version, bare)?
        }
        Commands::Add {
            id,
            source,
            version,
            side,
            group,
            no_lock,
        } => {
            let target = resolve_project_target(&root, &scope)?;
            cmd_add(&target.root, id, source, version, side, group, no_lock)?
        }
        Commands::Remove {
            id,
            source,
            group,
            all_groups,
            no_lock,
        } => {
            let target = resolve_project_target(&root, &scope)?;
            cmd_remove(&target.root, &id, source, group, all_groups, no_lock)?
        }
        Commands::Group { command } => {
            let target = resolve_project_target(&root, &scope)?;
            cmd_group(&target.root, command)?
        }
        Commands::Ls {
            status,
            info,
            groups,
            all_groups,
            json,
        } => {
            let target = resolve_project_target(&root, &scope)?;
            let selection = ProjectSelection {
                groups: &groups,
                all_groups,
                profiles: &scope.profiles,
                workspace: target.workspace.as_ref(),
                member_name: target.member_name.as_deref(),
            };
            cmd_ls(&target.root, status, info, no_color, json, selection)?
        }
        Commands::Search {
            query,
            source,
            limit,
            page,
            non_interactive,
            install_first,
            install_version,
            group,
        } => {
            let target = resolve_project_target(&root, &scope)?;
            cmd_search(
                &target.root,
                SearchCommandArgs {
                    query,
                    source,
                    limit,
                    page,
                    no_color,
                    non_interactive,
                    install_first,
                    install_version,
                    group,
                },
            )?
        }
        Commands::Tree {
            id,
            invert,
            all,
            source,
            groups,
            all_groups,
            json,
        } => {
            let target = resolve_project_target(&root, &scope)?;
            let selection = ProjectSelection {
                groups: &groups,
                all_groups,
                profiles: &scope.profiles,
                workspace: target.workspace.as_ref(),
                member_name: target.member_name.as_deref(),
            };
            cmd_tree(
                &target.root,
                TreeCommandArgs {
                    id,
                    invert,
                    all,
                    source,
                    json,
                },
                selection,
            )?
        }
        Commands::Why {
            id,
            source,
            groups,
            all_groups,
            json,
        } => {
            let target = resolve_project_target(&root, &scope)?;
            let selection = ProjectSelection {
                groups: &groups,
                all_groups,
                profiles: &scope.profiles,
                workspace: target.workspace.as_ref(),
                member_name: target.member_name.as_deref(),
            };
            cmd_why(&target.root, WhyCommandArgs { id, source, json }, selection)?
        }
        Commands::Update {
            id,
            source,
            to,
            groups,
            all_groups,
            no_lock,
        } => {
            let target = resolve_project_target(&root, &scope)?;
            cmd_update(&target.root, id, source, to, groups, all_groups, no_lock)?
        }
        Commands::Pin {
            id,
            source,
            version,
            groups,
            all_groups,
            no_lock,
        } => {
            let target = resolve_project_target(&root, &scope)?;
            cmd_pin(
                &target.root,
                id,
                source,
                version,
                groups,
                all_groups,
                no_lock,
            )?
        }
        Commands::Lock {
            command,
            upgrade,
            check,
            groups,
            all_groups,
        } => match command {
            Some(LockCommands::Diff { json }) => {
                if check {
                    bail!("`mineconda lock diff` does not accept `--check`");
                }
                let workspace = load_workspace_optional(&root)?;
                if workspace.is_some() && scope.all_members {
                    cmd_lock_diff_workspace(
                        &root,
                        upgrade,
                        groups,
                        all_groups,
                        &scope.profiles,
                        json,
                    )?
                } else {
                    let target = resolve_project_target(&root, &scope)?;
                    let selection = ProjectSelection {
                        groups: &groups,
                        all_groups,
                        profiles: &scope.profiles,
                        workspace: target.workspace.as_ref(),
                        member_name: target.member_name.as_deref(),
                    };
                    cmd_lock_diff(&target.root, upgrade, json, selection)?
                }
            }
            None => {
                let workspace = load_workspace_optional(&root)?;
                if workspace.is_some() && scope.all_members {
                    cmd_lock_workspace(&root, upgrade, check, groups, all_groups, &scope.profiles)?
                } else {
                    let target = resolve_project_target(&root, &scope)?;
                    cmd_lock(
                        &target.root,
                        upgrade,
                        check,
                        groups,
                        all_groups,
                        &scope.profiles,
                        target.workspace.as_ref(),
                    )?
                }
            }
        },
        Commands::Status {
            groups,
            all_groups,
            json,
        } => {
            let workspace = load_workspace_optional(&root)?;
            if workspace.is_some() && scope.all_members {
                cmd_status_workspace(&root, groups, all_groups, &scope.profiles, json)?
            } else {
                let target = resolve_project_target(&root, &scope)?;
                let selection = ProjectSelection {
                    groups: &groups,
                    all_groups,
                    profiles: &scope.profiles,
                    workspace: target.workspace.as_ref(),
                    member_name: target.member_name.as_deref(),
                };
                cmd_status(&target.root, json, selection)?
            }
        }
        Commands::Cache { command } => {
            let target = resolve_project_target(&root, &scope)?;
            cmd_cache(&target.root, command)?
        }
        Commands::Env { command } => cmd_env(&root, command, &scope)?,
        Commands::Sync {
            no_prune,
            check,
            locked,
            frozen,
            offline,
            jobs,
            verbose_cache,
            groups,
            all_groups,
        } => {
            let workspace = load_workspace_optional(&root)?;
            if workspace.is_some() && scope.all_members {
                cmd_sync_workspace(
                    &root,
                    SyncCommandArgs {
                        prune: !no_prune,
                        check,
                        locked: locked || frozen,
                        offline,
                        jobs,
                        verbose_cache,
                        groups,
                        all_groups,
                    },
                    &scope.profiles,
                )?
            } else {
                let target = resolve_project_target(&root, &scope)?;
                cmd_sync(
                    &target.root,
                    SyncCommandArgs {
                        prune: !no_prune,
                        check,
                        locked: locked || frozen,
                        offline,
                        jobs,
                        verbose_cache,
                        groups,
                        all_groups,
                    },
                    &scope.profiles,
                    target.workspace.as_ref(),
                )?
            }
        }
        Commands::Doctor { strict } => cmd_doctor(&root, strict, no_color)?,
        Commands::Run {
            dry_run,
            java,
            memory,
            jvm_args,
            mode,
            username,
            instance,
            launcher_jar,
            server_jar,
            groups,
            all_groups,
        } => {
            let workspace = load_workspace_optional(&root)?;
            if workspace.is_some() && scope.all_members {
                workspace_aggregation_not_supported("mineconda run")?;
            }
            let target = resolve_project_target(&root, &scope)?;
            cmd_run(
                &target.root,
                RunCommandArgs {
                    dry_run,
                    java,
                    memory,
                    jvm_args,
                    mode,
                    username,
                    instance,
                    launcher_jar,
                    server_jar,
                    groups,
                    all_groups,
                },
                &scope.profiles,
                target.workspace.as_ref(),
            )?
        }
        Commands::Export {
            format,
            output,
            groups,
            all_groups,
        } => {
            let workspace = load_workspace_optional(&root)?;
            if workspace.is_some() && scope.all_members {
                cmd_export_workspace(&root, format, output, groups, all_groups, &scope.profiles)?
            } else {
                let target = resolve_project_target(&root, &scope)?;
                cmd_export(
                    &target.root,
                    format,
                    output,
                    groups,
                    all_groups,
                    &scope.profiles,
                    target.workspace.as_ref(),
                )?
            }
        }
        Commands::Import {
            input,
            format,
            side,
            force,
        } => {
            let target = resolve_project_target(&root, &scope)?;
            cmd_import(&target.root, input, format, side, force)?
        }
    }

    Ok(())
}

fn init_modpack_layout(root: &Path) -> Result<()> {
    let directories = [
        "mods",
        "config",
        "defaultconfigs",
        "kubejs",
        "resourcepacks",
        "shaderpacks",
        "datapacks",
        "scripts",
        ".mineconda/cache/mods",
        ".mineconda/dev",
        ".mineconda/instances/dev",
    ];

    for dir in directories {
        fs::create_dir_all(root.join(dir))
            .with_context(|| format!("failed to create {}", root.join(dir).display()))?;
    }

    let keep_files = [
        "mods/.gitkeep",
        "config/.gitkeep",
        "defaultconfigs/.gitkeep",
        "kubejs/.gitkeep",
        "resourcepacks/.gitkeep",
        "shaderpacks/.gitkeep",
        "datapacks/.gitkeep",
        "scripts/.gitkeep",
    ];

    for keep in keep_files {
        write_file_if_missing(
            &root.join(keep),
            "# placeholder for keeping directory in version control\n",
        )?;
    }

    write_file_if_missing(
        &root.join(".gitignore"),
        "/logs/\n/crash-reports/\n/world/\n/server.jar\n/mods/*.jar\n!**/.gitkeep\n",
    )?;
    write_file_if_missing(
        &root.join("server.properties"),
        "# Generated by mineconda init\nspawn-protection=0\nmotd=Mineconda Server\nonline-mode=true\ndifficulty=normal\nmax-players=20\n",
    )?;
    write_file_if_missing(
        &root.join("eula.txt"),
        "# Set to true once you accept Mojang EULA\n# https://aka.ms/MinecraftEULA\neula=false\n",
    )?;
    write_file_if_missing(
        &root.join("start.sh"),
        "#!/usr/bin/env bash\nset -euo pipefail\nmineconda run \"$@\"\n",
    )?;
    write_file_if_missing(&root.join("start.bat"), "@echo off\nmineconda run %*\n")?;
    write_file_if_missing(
        &root.join(".mineconda/dev/README.txt"),
        "Place your client launcher jar here as .mineconda/dev/launcher.jar\n",
    )?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = root.join("start.sh");
        if script.exists() {
            let mut perm = fs::metadata(&script)?.permissions();
            perm.set_mode(0o755);
            fs::set_permissions(&script, perm)?;
        }
    }

    Ok(())
}

fn write_file_if_missing(path: &Path, content: &str) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}
