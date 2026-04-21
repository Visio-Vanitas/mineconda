use crate::*;
pub(crate) fn build_sync_report(
    root: &Path,
    args: SyncCommandArgs,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<CommandReport> {
    let SyncCommandArgs {
        prune,
        check,
        locked,
        offline,
        jobs,
        verbose_cache,
        groups,
        all_groups,
    } = args;
    if jobs == 0 {
        bail!("sync --jobs must be >= 1");
    }
    let manifest = load_manifest_optional(root)?;
    if check {
        return build_sync_check_report(
            root,
            manifest.as_ref(),
            SyncCommandArgs {
                prune,
                check,
                locked,
                offline,
                jobs,
                verbose_cache,
                groups,
                all_groups,
            },
            profiles,
            workspace,
        );
    }
    let mut lock = load_lockfile_required(root)?;
    let active_groups = if let Some(manifest) = manifest.as_ref() {
        let groups =
            activation_groups_with_profiles(manifest, workspace, &groups, all_groups, profiles)?;
        ensure_lock_covers_groups(manifest, &lock, &groups)?;
        groups
    } else if !groups.is_empty() || all_groups || !profiles.is_empty() {
        bail!("group/profile filters require a manifest");
    } else {
        BTreeSet::new()
    };
    let mut sync_lock = if active_groups.is_empty() {
        lock.clone()
    } else {
        filtered_lockfile(&lock, &active_groups)
    };
    let report = sync_lockfile(
        &mut sync_lock,
        &SyncRequest {
            project_root: root.to_path_buf(),
            prune,
            s3_cache: manifest
                .as_ref()
                .and_then(|manifest| manifest.cache.s3.clone()),
            offline,
            jobs,
            verbose_cache,
        },
    )?;

    let mut lines = Vec::new();
    if let Some(note) = experimental_s3_sync_note(manifest.as_ref(), report.s3_hits) {
        lines.push(note);
    }
    if report.lockfile_updated {
        if locked {
            bail!(
                "sync would update lockfile metadata in --locked/--frozen mode; run `mineconda sync` without lock guards first"
            );
        }
        if active_groups.is_empty() {
            lock = sync_lock;
        } else {
            merge_synced_lock_packages(&mut lock, &sync_lock);
        }
        let path = lockfile_path(root);
        write_lockfile(&path, &lock)
            .with_context(|| format!("failed to write {}", path.display()))?;
        lines.push(format!("lockfile metadata updated: {}", path.display()));
    }

    lines.push(format!(
        "sync done: packages={}, local_hits={}, s3_hits={}, origin_downloads={}, installed={}, removed={}, failed={}",
        report.package_count,
        report.local_hits,
        report.s3_hits,
        report.origin_downloads,
        report.installed,
        report.removed,
        report.failed
    ));

    Ok(CommandReport {
        output: format!("{}\n", lines.join("\n")),
        exit_code: 0,
    })
}

pub(crate) fn build_sync_json_report(
    root: &Path,
    args: SyncCommandArgs,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<SyncJsonReport> {
    let SyncCommandArgs {
        prune,
        check,
        locked,
        offline,
        jobs,
        verbose_cache,
        groups,
        all_groups,
    } = args;
    if jobs == 0 {
        bail!("sync --jobs must be >= 1");
    }

    let manifest = load_manifest_optional(root)?;
    if check {
        return build_sync_check_json_report(
            root,
            manifest.as_ref(),
            SyncCommandArgs {
                prune,
                check,
                locked,
                offline,
                jobs,
                verbose_cache,
                groups,
                all_groups,
            },
            profiles,
            workspace,
        );
    }

    let mut lock = load_lockfile_required(root)?;
    let active_groups = if let Some(manifest) = manifest.as_ref() {
        let groups =
            activation_groups_with_profiles(manifest, workspace, &groups, all_groups, profiles)?;
        ensure_lock_covers_groups(manifest, &lock, &groups)?;
        groups
    } else if !groups.is_empty() || all_groups || !profiles.is_empty() {
        bail!("group/profile filters require a manifest");
    } else {
        BTreeSet::new()
    };
    let profile_names = normalized_profile_names(profiles)?;
    let selected_groups = if active_groups.is_empty() {
        requested_groups_fallback(&groups, all_groups)
    } else {
        active_groups.iter().cloned().collect()
    };
    let mut sync_lock = if active_groups.is_empty() {
        lock.clone()
    } else {
        filtered_lockfile(&lock, &active_groups)
    };
    let report = sync_lockfile(
        &mut sync_lock,
        &SyncRequest {
            project_root: root.to_path_buf(),
            prune,
            s3_cache: manifest
                .as_ref()
                .and_then(|manifest| manifest.cache.s3.clone()),
            offline,
            jobs,
            verbose_cache,
        },
    )?;

    let mut messages = Vec::new();
    if let Some(note) = experimental_s3_sync_note(manifest.as_ref(), report.s3_hits) {
        messages.push(note);
    }
    let mut lockfile_updated = false;
    if report.lockfile_updated {
        if locked {
            bail!(
                "sync would update lockfile metadata in --locked/--frozen mode; run `mineconda sync` without lock guards first"
            );
        }
        if active_groups.is_empty() {
            lock = sync_lock;
        } else {
            merge_synced_lock_packages(&mut lock, &sync_lock);
        }
        let path = lockfile_path(root);
        write_lockfile(&path, &lock)
            .with_context(|| format!("failed to write {}", path.display()))?;
        messages.push(format!("lockfile metadata updated: {}", path.display()));
        lockfile_updated = true;
    }

    messages.push(format!(
        "sync done: packages={}, local_hits={}, s3_hits={}, origin_downloads={}, installed={}, removed={}, failed={}",
        report.package_count,
        report.local_hits,
        report.s3_hits,
        report.origin_downloads,
        report.installed,
        report.removed,
        report.failed
    ));

    Ok(SyncJsonReport {
        command: "sync",
        groups: selected_groups,
        profiles: profile_names,
        summary: SyncJsonSummary {
            mode: "sync",
            state: if report.failed > 0 {
                "partial"
            } else {
                "installed"
            },
            exit_code: 0,
            packages: report.package_count,
            installed: report.installed,
            missing: 0,
            local_hits: Some(report.local_hits),
            s3_hits: Some(report.s3_hits),
            origin_downloads: Some(report.origin_downloads),
            removed: Some(report.removed),
            failed: Some(report.failed),
            lockfile_updated,
        },
        missing_packages: Vec::new(),
        messages,
    })
}

pub(crate) fn cmd_sync(
    root: &Path,
    args: SyncCommandArgs,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<()> {
    emit_command_report(build_sync_report(root, args, profiles, workspace)?)
}

fn build_sync_check_json_report(
    root: &Path,
    manifest: Option<&Manifest>,
    args: SyncCommandArgs,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<SyncJsonReport> {
    let SyncCommandArgs {
        groups, all_groups, ..
    } = args;
    let lock_command = format_selection_command("mineconda lock", &groups, all_groups, profiles)?;
    let sync_command = format_selection_command("mineconda sync", &groups, all_groups, profiles)?;
    let profile_names = normalized_profile_names(profiles)?;
    let Some(lock) = load_lockfile_optional(root)? else {
        return Ok(SyncJsonReport {
            command: "sync",
            groups: requested_groups_fallback(&groups, all_groups),
            profiles: profile_names,
            summary: SyncJsonSummary {
                mode: "check",
                state: "error",
                exit_code: 1,
                packages: 0,
                installed: 0,
                missing: 0,
                local_hits: None,
                s3_hits: None,
                origin_downloads: None,
                removed: None,
                failed: None,
                lockfile_updated: false,
            },
            missing_packages: Vec::new(),
            messages: vec![format!(
                "sync check: lockfile missing; run `{lock_command}` first"
            )],
        });
    };
    let active_groups = if let Some(manifest) = manifest {
        let groups =
            activation_groups_with_profiles(manifest, workspace, &groups, all_groups, profiles)?;
        ensure_lock_covers_groups(manifest, &lock, &groups)?;
        groups
    } else if !groups.is_empty() || all_groups || !profiles.is_empty() {
        bail!("group/profile filters require a manifest");
    } else {
        BTreeSet::new()
    };
    let group_label = if active_groups.is_empty() {
        "all-locked".to_string()
    } else {
        format_active_groups(&active_groups)
    };
    let selected_groups = if active_groups.is_empty() {
        requested_groups_fallback(&groups, all_groups)
    } else {
        active_groups.iter().cloned().collect()
    };

    let filtered = if active_groups.is_empty() {
        lock.clone()
    } else {
        filtered_lockfile(&lock, &active_groups)
    };
    let mut packages: Vec<&LockedPackage> = filtered.packages.iter().collect();
    packages.sort_by(|left, right| {
        locked_package_graph_key(left).cmp(&locked_package_graph_key(right))
    });

    let mut missing = Vec::new();
    for package in packages {
        let target = package_install_target_path(root, package);
        if !target.exists() {
            missing.push((package, target));
        }
    }

    let package_count = filtered.packages.len();
    let installed = package_count.saturating_sub(missing.len());
    let mut messages = Vec::new();
    if let Some(note) = experimental_s3_sync_note(manifest, 0) {
        messages.push(note);
    }
    if missing.is_empty() {
        return Ok(SyncJsonReport {
            command: "sync",
            groups: selected_groups,
            profiles: profile_names,
            summary: SyncJsonSummary {
                mode: "check",
                state: "installed",
                exit_code: 0,
                packages: package_count,
                installed,
                missing: 0,
                local_hits: None,
                s3_hits: None,
                origin_downloads: None,
                removed: None,
                failed: None,
                lockfile_updated: false,
            },
            missing_packages: Vec::new(),
            messages: {
                messages.push(format!(
                    "sync check: installed groups={group_label} installed={installed} missing=0 packages={package_count}"
                ));
                messages
            },
        });
    }

    let missing_packages = missing
        .iter()
        .map(|(package, target)| SyncJsonMissingPackage {
            id: package.id.clone(),
            source: package.source.as_str().to_string(),
            version: package.version.clone(),
            target: target.display().to_string(),
            groups: normalized_package_groups(package),
        })
        .collect::<Vec<_>>();
    messages.push(format!(
        "sync check: missing groups={group_label} installed={installed} missing={} packages={package_count}",
        missing.len()
    ));
    for (package, target) in &missing {
        messages.push(format!(
            "- {} [{}] {} -> {} groups={}",
            package.id,
            package.source.as_str(),
            package.version,
            target.display(),
            format_group_list(&normalized_package_groups(package))
        ));
    }
    messages.push(format!("next: run `{sync_command}`"));

    Ok(SyncJsonReport {
        command: "sync",
        groups: selected_groups,
        profiles: profile_names,
        summary: SyncJsonSummary {
            mode: "check",
            state: "missing",
            exit_code: 2,
            packages: package_count,
            installed,
            missing: missing_packages.len(),
            local_hits: None,
            s3_hits: None,
            origin_downloads: None,
            removed: None,
            failed: None,
            lockfile_updated: false,
        },
        missing_packages,
        messages,
    })
}

pub(crate) fn build_sync_check_report(
    root: &Path,
    manifest: Option<&Manifest>,
    args: SyncCommandArgs,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<CommandReport> {
    let SyncCommandArgs {
        groups, all_groups, ..
    } = args;
    let lock_command = format_selection_command("mineconda lock", &groups, all_groups, profiles)?;
    let sync_command = format_selection_command("mineconda sync", &groups, all_groups, profiles)?;
    let Some(lock) = load_lockfile_optional(root)? else {
        return Ok(CommandReport {
            output: format!("sync check: lockfile missing; run `{lock_command}` first\n"),
            exit_code: 1,
        });
    };
    let active_groups = if let Some(manifest) = manifest {
        let groups =
            activation_groups_with_profiles(manifest, workspace, &groups, all_groups, profiles)?;
        ensure_lock_covers_groups(manifest, &lock, &groups)?;
        groups
    } else if !groups.is_empty() || all_groups || !profiles.is_empty() {
        bail!("group/profile filters require a manifest");
    } else {
        BTreeSet::new()
    };

    let filtered = if active_groups.is_empty() {
        lock.clone()
    } else {
        filtered_lockfile(&lock, &active_groups)
    };
    let mut packages: Vec<&LockedPackage> = filtered.packages.iter().collect();
    packages.sort_by(|left, right| {
        locked_package_graph_key(left).cmp(&locked_package_graph_key(right))
    });

    let mut missing = Vec::new();
    for package in packages {
        let target = package_install_target_path(root, package);
        if !target.exists() {
            missing.push((package, target));
        }
    }

    let package_count = filtered.packages.len();
    let installed = package_count.saturating_sub(missing.len());
    let group_label = if active_groups.is_empty() {
        "all-locked".to_string()
    } else {
        format_active_groups(&active_groups)
    };
    let s3_note = experimental_s3_sync_note(manifest, 0);

    if missing.is_empty() {
        let mut lines = Vec::new();
        if let Some(note) = s3_note {
            lines.push(note);
        }
        lines.push(format!(
            "sync check: installed groups={group_label} installed={installed} missing=0 packages={package_count}"
        ));
        return Ok(CommandReport {
            output: format!("{}\n", lines.join("\n")),
            exit_code: 0,
        });
    }

    let mut lines = Vec::new();
    if let Some(note) = s3_note {
        lines.push(note);
    }
    lines.push(format!(
        "sync check: missing groups={group_label} installed={installed} missing={} packages={package_count}",
        missing.len()
    ));
    for (package, target) in missing {
        lines.push(format!(
            "- {} [{}] {} -> {} groups={}",
            package.id,
            package.source.as_str(),
            package.version,
            target.display(),
            format_group_list(&normalized_package_groups(package))
        ));
    }
    lines.push(format!("next: run `{sync_command}`"));
    Ok(CommandReport {
        output: format!("{}\n", lines.join("\n")),
        exit_code: 2,
    })
}

fn merge_synced_lock_packages(lock: &mut Lockfile, synced: &Lockfile) {
    let synced_by_key: HashMap<String, &LockedPackage> = synced
        .packages
        .iter()
        .map(|package| (locked_package_graph_key(package), package))
        .collect();

    for package in &mut lock.packages {
        let key = locked_package_graph_key(package);
        if let Some(updated) = synced_by_key.get(&key) {
            *package = (*updated).clone();
        }
    }
}

fn experimental_s3_sync_note(manifest: Option<&Manifest>, s3_hits: usize) -> Option<String> {
    let manifest = manifest?;
    let has_s3_source = manifest
        .mods
        .iter()
        .any(|entry| entry.source == ModSource::S3);
    let has_s3_cache = manifest
        .cache
        .s3
        .as_ref()
        .is_some_and(|cache| cache.enabled);
    if !(has_s3_source || has_s3_cache) {
        return None;
    }

    let mut note =
        "sync note: experimental S3 source/cache path is configured; validate it with `mineconda doctor` and the optional remote smoke before relying on it".to_string();
    if s3_hits > 0 {
        note.push_str(&format!(" (s3_hits={s3_hits})"));
    }
    Some(note)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use mineconda_core::{
        DEFAULT_GROUP_NAME, ModSide, ModSource, ModSpec, manifest_path, write_manifest,
    };
    use mineconda_resolver::{ResolveRequest, resolve_lockfile};

    use super::*;
    use crate::cli::SyncCommandArgs;
    use crate::test_support::TempProject;

    #[test]
    fn sync_check_report_detects_missing_and_installed_packages() {
        let project = TempProject::new("sync-check-installed");
        fs::create_dir_all(project.path.join("vendor")).expect("vendor dir");
        fs::write(project.path.join("vendor/demo.jar"), b"demo").expect("fixture jar");
        let manifest = crate::test_support::test_manifest(vec![ModSpec::new(
            "demo".to_string(),
            ModSource::Local,
            "vendor/demo.jar".to_string(),
            ModSide::Both,
        )]);
        write_manifest(&manifest_path(&project.path), &manifest).expect("write manifest");
        let output = resolve_lockfile(
            &manifest,
            None,
            &ResolveRequest {
                upgrade: false,
                groups: BTreeSet::from([DEFAULT_GROUP_NAME.to_string()]),
            },
        )
        .expect("resolve lock");
        write_lockfile(&lockfile_path(&project.path), &output.lockfile).expect("write lock");

        let missing = build_sync_check_report(
            &project.path,
            Some(&manifest),
            SyncCommandArgs {
                prune: true,
                check: true,
                locked: false,
                offline: false,
                jobs: 1,
                verbose_cache: false,
                groups: Vec::new(),
                all_groups: false,
            },
            &[],
            None,
        )
        .expect("sync check report");
        assert_eq!(missing.exit_code, 2);
        assert!(missing.output.contains("sync check: missing"));
        assert!(missing.output.contains("run `mineconda sync`"));

        fs::create_dir_all(project.path.join("mods")).expect("mods dir");
        let package = output.lockfile.packages.first().expect("package");
        fs::write(
            project.path.join(package.install_path_or_default()),
            b"installed",
        )
        .expect("installed package");

        let installed = build_sync_check_report(
            &project.path,
            Some(&manifest),
            SyncCommandArgs {
                prune: true,
                check: true,
                locked: false,
                offline: false,
                jobs: 1,
                verbose_cache: false,
                groups: Vec::new(),
                all_groups: false,
            },
            &[],
            None,
        )
        .expect("sync check report");
        assert_eq!(installed.exit_code, 0);
        assert!(installed.output.contains("sync check: installed"));
    }

    #[test]
    fn sync_json_report_describes_missing_packages() {
        let project = TempProject::new("sync-json-missing");
        fs::create_dir_all(project.path.join("vendor")).expect("vendor dir");
        fs::write(project.path.join("vendor/demo.jar"), b"demo").expect("fixture jar");
        let manifest = crate::test_support::test_manifest(vec![ModSpec::new(
            "demo".to_string(),
            ModSource::Local,
            "vendor/demo.jar".to_string(),
            ModSide::Both,
        )]);
        write_manifest(&manifest_path(&project.path), &manifest).expect("write manifest");
        let output = resolve_lockfile(
            &manifest,
            None,
            &ResolveRequest {
                upgrade: false,
                groups: BTreeSet::from([DEFAULT_GROUP_NAME.to_string()]),
            },
        )
        .expect("resolve lock");
        write_lockfile(&lockfile_path(&project.path), &output.lockfile).expect("write lock");

        let report = build_sync_json_report(
            &project.path,
            SyncCommandArgs {
                prune: true,
                check: true,
                locked: false,
                offline: false,
                jobs: 1,
                verbose_cache: false,
                groups: Vec::new(),
                all_groups: false,
            },
            &[],
            None,
        )
        .expect("sync json report");

        assert_eq!(report.summary.mode, "check");
        assert_eq!(report.summary.state, "missing");
        assert_eq!(report.summary.exit_code, 2);
        assert_eq!(report.summary.packages, 1);
        assert_eq!(report.summary.installed, 0);
        assert_eq!(report.summary.missing, 1);
        assert_eq!(report.missing_packages.len(), 1);
        assert_eq!(report.missing_packages[0].id, "demo");
        assert!(
            report
                .messages
                .iter()
                .any(|line| line.contains("next: run `mineconda sync`")),
            "messages should suggest running sync: {:?}",
            report.messages
        );
    }

    #[test]
    fn sync_json_report_marks_experimental_s3_usage() {
        let project = TempProject::new("sync-json-s3-note");
        fs::create_dir_all(project.path.join("vendor")).expect("vendor dir");
        fs::write(project.path.join("vendor/demo.jar"), b"demo").expect("fixture jar");
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

[cache.s3]
enabled = true
bucket = "cache-bucket"
auth = "auto"
"#,
        )
        .expect("manifest");
        write_manifest(&manifest_path(&project.path), &manifest).expect("write manifest");
        let output = resolve_lockfile(
            &manifest,
            None,
            &ResolveRequest {
                upgrade: false,
                groups: BTreeSet::from([DEFAULT_GROUP_NAME.to_string()]),
            },
        )
        .expect("resolve lock");
        write_lockfile(&lockfile_path(&project.path), &output.lockfile).expect("write lock");

        let report = build_sync_json_report(
            &project.path,
            SyncCommandArgs {
                prune: true,
                check: true,
                locked: false,
                offline: false,
                jobs: 1,
                verbose_cache: false,
                groups: Vec::new(),
                all_groups: false,
            },
            &[],
            None,
        )
        .expect("sync json report");

        assert!(
            report
                .messages
                .iter()
                .any(|line| { line.contains("experimental S3 source/cache path is configured") })
        );
    }

    #[test]
    fn sync_json_report_skips_disabled_s3_cache_note() {
        let project = TempProject::new("sync-json-s3-disabled-note");
        fs::create_dir_all(project.path.join("vendor")).expect("vendor dir");
        fs::write(project.path.join("vendor/demo.jar"), b"demo").expect("fixture jar");
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

[cache.s3]
enabled = false
bucket = "cache-bucket"
auth = "auto"
"#,
        )
        .expect("manifest");
        write_manifest(&manifest_path(&project.path), &manifest).expect("write manifest");
        let output = resolve_lockfile(
            &manifest,
            None,
            &ResolveRequest {
                upgrade: false,
                groups: BTreeSet::from([DEFAULT_GROUP_NAME.to_string()]),
            },
        )
        .expect("resolve lock");
        write_lockfile(&lockfile_path(&project.path), &output.lockfile).expect("write lock");

        let report = build_sync_json_report(
            &project.path,
            SyncCommandArgs {
                prune: true,
                check: true,
                locked: false,
                offline: false,
                jobs: 1,
                verbose_cache: false,
                groups: Vec::new(),
                all_groups: false,
            },
            &[],
            None,
        )
        .expect("sync json report");

        assert!(
            report
                .messages
                .iter()
                .all(|line| { !line.contains("experimental S3 source/cache path is configured") })
        );
    }

    #[test]
    fn sync_check_ignores_inactive_group_entries() {
        let project = TempProject::new("sync-check-inactive-groups");
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

        let report = build_sync_check_report(
            &project.path,
            Some(&manifest),
            SyncCommandArgs {
                prune: true,
                check: true,
                locked: false,
                offline: false,
                jobs: 1,
                verbose_cache: false,
                groups: Vec::new(),
                all_groups: false,
            },
            &[],
            None,
        )
        .expect("sync check report");
        assert_eq!(report.exit_code, 0);
        assert!(report.output.contains("sync check: installed"));
    }

    #[test]
    fn sync_report_installs_local_packages_and_updates_lockfile() {
        let project = TempProject::new("sync-report-install");
        fs::create_dir_all(project.path.join("vendor")).expect("vendor dir");
        fs::write(project.path.join("vendor/demo.jar"), b"demo").expect("fixture jar");
        let manifest = crate::test_support::test_manifest(vec![ModSpec::new(
            "demo".to_string(),
            ModSource::Local,
            "vendor/demo.jar".to_string(),
            ModSide::Both,
        )]);
        write_manifest(&manifest_path(&project.path), &manifest).expect("write manifest");
        let output = resolve_lockfile(
            &manifest,
            None,
            &ResolveRequest {
                upgrade: false,
                groups: BTreeSet::from([DEFAULT_GROUP_NAME.to_string()]),
            },
        )
        .expect("resolve lock");
        write_lockfile(&lockfile_path(&project.path), &output.lockfile).expect("write lock");

        let report = build_sync_report(
            &project.path,
            SyncCommandArgs {
                prune: true,
                check: false,
                locked: false,
                offline: false,
                jobs: 1,
                verbose_cache: false,
                groups: Vec::new(),
                all_groups: false,
            },
            &[],
            None,
        )
        .expect("sync report");

        assert_eq!(report.exit_code, 0);
        assert!(report.output.contains("sync done: packages=1"));
        assert!(project.path.join("mods/demo.jar").exists());
        let updated_lock = load_lockfile_required(&project.path).expect("updated lock");
        assert_eq!(updated_lock.packages.len(), 1);
    }
}
