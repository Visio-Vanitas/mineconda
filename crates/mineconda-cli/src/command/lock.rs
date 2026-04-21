use crate::*;
pub(crate) fn cmd_lock(
    root: &Path,
    upgrade: bool,
    check: bool,
    groups: Vec<String>,
    all_groups: bool,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<()> {
    let manifest = load_manifest(root)?;
    if check {
        return emit_command_report(build_lock_check_report(
            root, &manifest, upgrade, groups, all_groups, profiles, workspace,
        )?);
    }
    write_lock_from_manifest(
        root,
        &manifest,
        upgrade,
        activation_groups_with_profiles(&manifest, workspace, &groups, all_groups, profiles)?,
    )
}

pub(crate) fn build_lock_check_report(
    root: &Path,
    manifest: &Manifest,
    upgrade: bool,
    groups: Vec<String>,
    all_groups: bool,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<CommandReport> {
    let lock_command = format_selection_command("mineconda lock", &groups, all_groups, profiles)?;
    let active_groups =
        activation_groups_with_profiles(manifest, workspace, &groups, all_groups, profiles)?;
    let group_label = format_active_groups(&active_groups);
    let Some(lock) = load_lockfile_optional(root)? else {
        return Ok(CommandReport {
            output: format!("lock check: lockfile missing; run `{lock_command}` first\n"),
            exit_code: 1,
        });
    };

    if !lock.metadata.dependency_graph {
        return Ok(CommandReport {
            output: format!(
                "lock check: lockfile does not contain dependency graph data; rerun `{lock_command}` first\n"
            ),
            exit_code: 1,
        });
    }

    if let Err(err) = ensure_lock_group_metadata(manifest, &lock, &active_groups) {
        return Ok(CommandReport {
            output: format!("lock check: {err}\n"),
            exit_code: 1,
        });
    }

    let output = resolve_lockfile(
        manifest,
        Some(&lock),
        &ResolveRequest {
            upgrade,
            groups: active_groups.clone(),
        },
    )?;
    let current_for_diff = filtered_lockfile(&lock, &active_groups);
    let entries = compute_lock_diff_entries(Some(&current_for_diff), &output.lockfile);
    let report = LockDiffJsonReport {
        command: "lock-diff",
        groups: active_groups.iter().cloned().collect(),
        summary: LockDiffJsonSummary {
            install: output.plan.install.len(),
            remove: output.plan.remove.len(),
            unchanged: output.plan.unchanged.len(),
            changes: entries.len(),
        },
        entries: entries.iter().map(lock_diff_entry_to_json).collect(),
    };

    if report.entries.is_empty() {
        return Ok(CommandReport {
            output: format!(
                "lock check: up-to-date groups={group_label} install={} remove={} unchanged={} changes=0\n",
                report.summary.install, report.summary.remove, report.summary.unchanged
            ),
            exit_code: 0,
        });
    }

    let mut lines = vec![format!(
        "lock check: stale groups={group_label} install={} remove={} unchanged={} changes={}",
        report.summary.install,
        report.summary.remove,
        report.summary.unchanged,
        report.summary.changes
    )];
    lines.extend(lock_diff_report_body_lines(&report));
    Ok(CommandReport {
        output: format!("{}\n", lines.join("\n")),
        exit_code: 2,
    })
}

fn command_display_name(command: &str) -> &str {
    match command {
        "lock-diff" => "lock diff",
        "status" => "status",
        _ => command,
    }
}

pub(crate) fn json_error_report(
    command: &'static str,
    groups: Vec<String>,
    error: impl Into<String>,
    exit_code: i32,
) -> JsonErrorReport {
    JsonErrorReport {
        command,
        groups,
        error: format!("{}: {}", command_display_name(command), error.into()),
        exit_code,
    }
}

pub(crate) fn render_json_error_report(report: &JsonErrorReport) -> CommandReport {
    CommandReport {
        output: format!("{}\n", report.error),
        exit_code: report.exit_code,
    }
}

pub(crate) fn lock_diff_report_body_lines(report: &LockDiffJsonReport) -> Vec<String> {
    if report.entries.is_empty() {
        return vec!["lock diff: no changes".to_string()];
    }

    report
        .entries
        .iter()
        .map(|entry| {
            render_lock_diff_entry(&LockDiffEntry {
                kind: match entry.kind.as_str() {
                    "add" => LockDiffKind::Add,
                    "remove" => LockDiffKind::Remove,
                    "upgrade" => LockDiffKind::Upgrade,
                    "downgrade" => LockDiffKind::Downgrade,
                    "change_version" => LockDiffKind::ChangeVersion,
                    "change_artifact" => LockDiffKind::ChangeArtifact,
                    "change_groups" => LockDiffKind::ChangeGroups,
                    "change_dependencies" => LockDiffKind::ChangeDependencies,
                    _ => LockDiffKind::ChangeVersion,
                },
                id: entry.id.clone(),
                source: match entry.source.as_str() {
                    "modrinth" => ModSource::Modrinth,
                    "curseforge" => ModSource::Curseforge,
                    "url" => ModSource::Url,
                    "local" => ModSource::Local,
                    "s3" => ModSource::S3,
                    _ => ModSource::Local,
                },
                current_version: entry.current_version.clone(),
                desired_version: entry.desired_version.clone(),
                current_groups: entry.current_groups.clone(),
                desired_groups: entry.desired_groups.clone(),
                current_dependencies: entry
                    .current_dependencies
                    .iter()
                    .map(|dep| LockedDependency {
                        source: match dep.source.as_str() {
                            "modrinth" => ModSource::Modrinth,
                            "curseforge" => ModSource::Curseforge,
                            "url" => ModSource::Url,
                            "local" => ModSource::Local,
                            "s3" => ModSource::S3,
                            _ => ModSource::Local,
                        },
                        id: dep.id.clone(),
                        kind: match dep.kind.as_str() {
                            "required" => LockedDependencyKind::Required,
                            "incompatible" => LockedDependencyKind::Incompatible,
                            _ => LockedDependencyKind::Required,
                        },
                        constraint: dep.constraint.clone(),
                    })
                    .collect(),
                desired_dependencies: entry
                    .desired_dependencies
                    .iter()
                    .map(|dep| LockedDependency {
                        source: match dep.source.as_str() {
                            "modrinth" => ModSource::Modrinth,
                            "curseforge" => ModSource::Curseforge,
                            "url" => ModSource::Url,
                            "local" => ModSource::Local,
                            "s3" => ModSource::S3,
                            _ => ModSource::Local,
                        },
                        id: dep.id.clone(),
                        kind: match dep.kind.as_str() {
                            "required" => LockedDependencyKind::Required,
                            "incompatible" => LockedDependencyKind::Incompatible,
                            _ => LockedDependencyKind::Required,
                        },
                        constraint: dep.constraint.clone(),
                    })
                    .collect(),
                current_artifact: entry.current_artifact.clone(),
                desired_artifact: entry.desired_artifact.clone(),
            })
        })
        .collect()
}

pub(crate) fn render_lock_diff_json_report(report: &LockDiffJsonReport) -> CommandReport {
    let mut lines = vec![format!(
        "lock diff: groups={} install={} remove={} unchanged={} changes={}",
        format_group_list(&report.groups),
        report.summary.install,
        report.summary.remove,
        report.summary.unchanged,
        report.summary.changes
    )];
    if report.entries.is_empty() {
        lines.push("lock diff: no changes".to_string());
        return CommandReport {
            output: format!("{}\n", lines.join("\n")),
            exit_code: 0,
        };
    }

    lines.extend(lock_diff_report_body_lines(report));

    CommandReport {
        output: format!("{}\n", lines.join("\n")),
        exit_code: 2,
    }
}

pub(crate) fn render_status_json_report(report: &StatusJsonReport) -> CommandReport {
    let mut lines = vec![if report.summary.state == "drift" {
        "status summary: drift detected".to_string()
    } else {
        "status summary: clean".to_string()
    }];
    lines.extend(report.messages.clone());
    CommandReport {
        output: format!("{}\n", lines.join("\n")),
        exit_code: report.summary.exit_code,
    }
}

pub(crate) fn build_lock_diff_json_report(
    root: &Path,
    upgrade: bool,
    groups: Vec<String>,
    all_groups: bool,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<std::result::Result<LockDiffJsonReport, JsonErrorReport>> {
    let manifest = load_manifest(root)?;
    let active_groups =
        activation_groups_with_profiles(&manifest, workspace, &groups, all_groups, profiles)?;
    let current = load_lockfile_optional(root)?;
    let groups = active_groups.iter().cloned().collect::<Vec<_>>();

    if let Some(lock) = current.as_ref() {
        if !lock.metadata.dependency_graph {
            return Ok(Err(json_error_report(
                "lock-diff",
                groups,
                "lockfile does not contain dependency graph data; rerun `mineconda lock` first",
                2,
            )));
        }
        if let Err(err) = ensure_lock_group_metadata(&manifest, lock, &active_groups) {
            return Ok(Err(json_error_report(
                "lock-diff",
                groups,
                err.to_string(),
                2,
            )));
        }
    }

    let output = resolve_lockfile(
        &manifest,
        current.as_ref(),
        &ResolveRequest {
            upgrade,
            groups: active_groups.clone(),
        },
    )?;
    let current_for_diff = current
        .as_ref()
        .map(|lock| filtered_lockfile(lock, &active_groups));
    let entries = compute_lock_diff_entries(current_for_diff.as_ref(), &output.lockfile);
    Ok(Ok(LockDiffJsonReport {
        command: "lock-diff",
        groups,
        summary: LockDiffJsonSummary {
            install: output.plan.install.len(),
            remove: output.plan.remove.len(),
            unchanged: output.plan.unchanged.len(),
            changes: entries.len(),
        },
        entries: entries.iter().map(lock_diff_entry_to_json).collect(),
    }))
}

pub(crate) fn cmd_lock_diff(
    root: &Path,
    upgrade: bool,
    json: bool,
    selection: ProjectSelection<'_>,
) -> Result<()> {
    let groups = selection.groups.to_vec();
    let fallback_groups = selection.fallback_groups();
    if json {
        match build_lock_diff_json_report(
            root,
            upgrade,
            groups.clone(),
            selection.all_groups,
            selection.profiles,
            selection.workspace,
        ) {
            Ok(Ok(report)) => {
                let exit_code = if report.entries.is_empty() { 0 } else { 2 };
                emit_json_report(&report, exit_code)
            }
            Ok(Err(error)) => emit_json_report(&error, error.exit_code),
            Err(err) => emit_json_report(
                &json_error_report("lock-diff", fallback_groups, format!("{err:#}"), 1),
                1,
            ),
        }
    } else {
        match build_lock_diff_json_report(
            root,
            upgrade,
            groups,
            selection.all_groups,
            selection.profiles,
            selection.workspace,
        )? {
            Ok(report) => emit_command_report(render_lock_diff_json_report(&report)),
            Err(error) => emit_command_report(render_json_error_report(&error)),
        }
    }
}

pub(crate) fn build_lock_write_report(
    root: &Path,
    manifest: &Manifest,
    upgrade: bool,
    groups: BTreeSet<String>,
) -> Result<LockWriteReport> {
    let old_lock = load_lockfile_optional(root)?;
    let output = resolve_lockfile(
        manifest,
        old_lock.as_ref(),
        &ResolveRequest { upgrade, groups },
    )?;
    let path = lockfile_path(root);
    write_lockfile(&path, &output.lockfile)
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(LockWriteReport {
        install: output.plan.install.len(),
        remove: output.plan.remove.len(),
        unchanged: output.plan.unchanged.len(),
    })
}

pub(crate) fn write_lock_from_manifest(
    root: &Path,
    manifest: &Manifest,
    upgrade: bool,
    groups: BTreeSet<String>,
) -> Result<()> {
    emit_command_report(render_lock_write_report(build_lock_write_report(
        root, manifest, upgrade, groups,
    )?))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::compute_lock_diff_entries;
    use crate::report::{LockDiffJsonEntry, LockDiffJsonReport, LockDiffJsonSummary};
    use crate::test_support::{
        TempProject, required_dependency, test_lockfile, test_manifest, test_package,
    };
    use mineconda_core::{
        DEFAULT_GROUP_NAME, LockedPackage, ModSide, ModSource, ModSpec, lockfile_path,
        manifest_path, write_lockfile, write_manifest,
    };

    #[test]
    fn lock_diff_entries_capture_upgrade_group_and_dependency_changes() {
        let mut current = test_package("alpha", "1.0.0", vec![required_dependency("beta")]);
        current.groups = vec![DEFAULT_GROUP_NAME.to_string()];

        let mut desired = test_package("alpha", "2.0.0", vec![required_dependency("gamma")]);
        desired.groups = vec![DEFAULT_GROUP_NAME.to_string(), "client".to_string()];

        let entries = compute_lock_diff_entries(
            Some(&test_lockfile(vec![current])),
            &test_lockfile(vec![desired]),
        );

        assert!(
            entries
                .iter()
                .any(|entry| entry.kind == LockDiffKind::Upgrade)
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.kind == LockDiffKind::ChangeGroups)
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.kind == LockDiffKind::ChangeDependencies)
        );
    }

    #[test]
    fn lock_check_report_is_clean_for_matching_lockfile() {
        let project = TempProject::new("lock-check-clean");
        fs::create_dir_all(project.path.join("vendor")).expect("vendor dir");
        fs::write(project.path.join("vendor/demo.jar"), b"demo").expect("fixture jar");
        let manifest = test_manifest(vec![ModSpec::new(
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
        .expect("resolve local lock");
        write_lockfile(&lockfile_path(&project.path), &output.lockfile).expect("write lock");

        let report = build_lock_check_report(
            &project.path,
            &manifest,
            false,
            Vec::new(),
            false,
            &[],
            None,
        )
        .expect("lock check report");
        assert_eq!(report.exit_code, 0);
        assert!(report.output.contains("lock check: up-to-date"));
    }

    #[test]
    fn lock_check_report_marks_manifest_drift() {
        let project = TempProject::new("lock-check-drift");
        fs::create_dir_all(project.path.join("vendor")).expect("vendor dir");
        fs::write(project.path.join("vendor/demo.jar"), b"demo").expect("demo jar");
        fs::write(project.path.join("vendor/extra.jar"), b"extra").expect("extra jar");
        let manifest = test_manifest(vec![ModSpec::new(
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
        .expect("resolve local lock");
        write_lockfile(&lockfile_path(&project.path), &output.lockfile).expect("write lock");

        let updated_manifest = test_manifest(vec![
            ModSpec::new(
                "demo".to_string(),
                ModSource::Local,
                "vendor/demo.jar".to_string(),
                ModSide::Both,
            ),
            ModSpec::new(
                "extra".to_string(),
                ModSource::Local,
                "vendor/extra.jar".to_string(),
                ModSide::Both,
            ),
        ]);
        write_manifest(&manifest_path(&project.path), &updated_manifest).expect("rewrite manifest");

        let report = build_lock_check_report(
            &project.path,
            &updated_manifest,
            false,
            Vec::new(),
            false,
            &[],
            None,
        )
        .expect("lock check report");
        assert_eq!(report.exit_code, 2);
        assert!(report.output.contains("lock check: stale"));
        assert!(report.output.contains("ADD extra [local]"));
    }

    #[test]
    fn lock_check_report_requires_existing_lockfile() {
        let project = TempProject::new("lock-check-missing");
        fs::create_dir_all(project.path.join("vendor")).expect("vendor dir");
        fs::write(project.path.join("vendor/demo.jar"), b"demo").expect("fixture jar");
        let manifest = test_manifest(vec![ModSpec::new(
            "demo".to_string(),
            ModSource::Local,
            "vendor/demo.jar".to_string(),
            ModSide::Both,
        )]);
        write_manifest(&manifest_path(&project.path), &manifest).expect("write manifest");

        let report = build_lock_check_report(
            &project.path,
            &manifest,
            false,
            Vec::new(),
            false,
            &[],
            None,
        )
        .expect("lock check report");
        assert_eq!(report.exit_code, 1);
        assert!(report.output.contains("lock check: lockfile missing"));
        assert!(report.output.contains("run `mineconda lock` first"));
    }

    #[test]
    fn lock_check_report_requires_dependency_graph_metadata() {
        let project = TempProject::new("lock-check-metadata");
        let manifest = test_manifest(vec![ModSpec::new(
            "demo".to_string(),
            ModSource::Local,
            "vendor/demo.jar".to_string(),
            ModSide::Both,
        )]);
        fs::create_dir_all(project.path.join("vendor")).expect("vendor dir");
        fs::write(project.path.join("vendor/demo.jar"), b"demo").expect("fixture jar");
        write_manifest(&manifest_path(&project.path), &manifest).expect("write manifest");

        let mut lock = test_lockfile(vec![LockedPackage::placeholder(&manifest.mods[0])]);
        lock.metadata.dependency_graph = false;
        write_lockfile(&lockfile_path(&project.path), &lock).expect("write lock");

        let report = build_lock_check_report(
            &project.path,
            &manifest,
            false,
            Vec::new(),
            false,
            &[],
            None,
        )
        .expect("lock check report");
        assert_eq!(report.exit_code, 1);
        assert!(report.output.contains("dependency graph data"));
    }

    #[test]
    fn lock_diff_report_requires_dependency_graph_metadata() {
        let project = TempProject::new("lock-diff-metadata");
        let manifest = test_manifest(vec![ModSpec::new(
            "demo".to_string(),
            ModSource::Local,
            "vendor/demo.jar".to_string(),
            ModSide::Both,
        )]);
        fs::create_dir_all(project.path.join("vendor")).expect("vendor dir");
        fs::write(project.path.join("vendor/demo.jar"), b"demo").expect("fixture jar");
        write_manifest(&manifest_path(&project.path), &manifest).expect("write manifest");

        let mut lock = test_lockfile(vec![LockedPackage::placeholder(&manifest.mods[0])]);
        lock.metadata.dependency_graph = false;
        write_lockfile(&lockfile_path(&project.path), &lock).expect("write lock");

        let report =
            build_lock_diff_json_report(&project.path, false, Vec::new(), false, &[], None)
                .expect("report")
                .expect_err("expected metadata error");
        assert_eq!(report.command, "lock-diff");
        assert_eq!(report.exit_code, 2);
        assert!(report.error.contains("dependency graph data"));
    }

    #[test]
    fn lock_diff_text_report_renders_from_json_report() {
        let report = LockDiffJsonReport {
            command: "lock-diff",
            groups: vec![DEFAULT_GROUP_NAME.to_string()],
            summary: LockDiffJsonSummary {
                install: 1,
                remove: 0,
                unchanged: 0,
                changes: 1,
            },
            entries: vec![LockDiffJsonEntry {
                kind: "add".to_string(),
                id: "iris".to_string(),
                source: "modrinth".to_string(),
                current_version: None,
                desired_version: Some("1.0.0".to_string()),
                current_groups: Vec::new(),
                desired_groups: vec![DEFAULT_GROUP_NAME.to_string()],
                current_dependencies: Vec::new(),
                desired_dependencies: Vec::new(),
                current_artifact: None,
                desired_artifact: Some("file=iris.jar".to_string()),
            }],
        };

        let rendered = render_lock_diff_json_report(&report);
        assert_eq!(rendered.exit_code, 2);
        assert!(rendered.output.contains("lock diff: groups=default"));
        assert!(
            rendered
                .output
                .contains("ADD iris [modrinth] -> 1.0.0 groups=default")
        );
    }
}
