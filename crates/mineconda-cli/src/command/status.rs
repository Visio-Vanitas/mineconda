use crate::command::lock::json_error_report;
use crate::*;
pub(crate) fn build_status_json_report(
    root: &Path,
    groups: Vec<String>,
    all_groups: bool,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<StatusJsonReport> {
    let manifest_path = manifest_path(root);
    let lock_path = lockfile_path(root);
    let lock_command = format_selection_command("mineconda lock", &groups, all_groups, profiles)?;
    let sync_command = format_selection_command("mineconda sync", &groups, all_groups, profiles)?;
    let manifest = load_manifest_optional(root)?;
    let lock = load_lockfile_optional(root)?;
    let mut messages = Vec::new();

    let Some(manifest) = manifest else {
        let groups = requested_groups_fallback(&groups, all_groups);
        messages.push(format!("manifest: missing ({})", manifest_path.display()));
        let lockfile = match lock {
            Some(lock) => {
                messages.push(format!(
                    "lockfile: {} (packages={})",
                    lock_path.display(),
                    lock.packages.len()
                ));
                StatusJsonLockfile {
                    exists: true,
                    path: lock_path.display().to_string(),
                    packages: Some(lock.packages.len()),
                    dependency_graph: Some(lock.metadata.dependency_graph),
                    group_metadata: Some(lock.metadata.group_metadata),
                }
            }
            None => {
                messages.push(format!("lockfile: missing ({})", lock_path.display()));
                StatusJsonLockfile {
                    exists: false,
                    path: lock_path.display().to_string(),
                    packages: None,
                    dependency_graph: None,
                    group_metadata: None,
                }
            }
        };
        return Ok(StatusJsonReport {
            command: "status",
            groups,
            summary: StatusJsonSummary {
                state: "drift",
                exit_code: 2,
            },
            manifest: StatusJsonManifest {
                exists: false,
                path: manifest_path.display().to_string(),
                roots: None,
                named_groups: None,
            },
            lockfile,
            checks: StatusJsonChecks {
                project_metadata: "unavailable",
                group_coverage: "unavailable",
                resolution: "unavailable",
                sync: StatusJsonSync {
                    installed: None,
                    missing: None,
                    packages: None,
                },
            },
            messages,
        });
    };

    let active_groups =
        activation_groups_with_profiles(&manifest, workspace, &groups, all_groups, profiles)?;
    let groups = active_groups.iter().cloned().collect::<Vec<_>>();
    let selected_specs = selected_manifest_specs(&manifest, &active_groups);
    let profile_names = normalized_profile_names(profiles)?;
    if !profile_names.is_empty() {
        messages.push(format!("status: profiles={}", profile_names.join(",")));
    }
    messages.push(format!(
        "status: groups={}",
        format_active_groups(&active_groups)
    ));
    messages.push(format!(
        "manifest: {} (roots={}, named-groups={})",
        manifest_path.display(),
        selected_specs.len(),
        manifest.groups.0.len()
    ));

    let Some(lock) = lock else {
        messages.push(format!("lockfile: missing ({})", lock_path.display()));
        messages.push(format!(
            "resolution: lockfile missing; run `{lock_command}`"
        ));
        messages.push("sync: unavailable until a lockfile exists".to_string());
        return Ok(StatusJsonReport {
            command: "status",
            groups,
            summary: StatusJsonSummary {
                state: "drift",
                exit_code: 2,
            },
            manifest: StatusJsonManifest {
                exists: true,
                path: manifest_path.display().to_string(),
                roots: Some(selected_specs.len()),
                named_groups: Some(manifest.groups.0.len()),
            },
            lockfile: StatusJsonLockfile {
                exists: false,
                path: lock_path.display().to_string(),
                packages: None,
                dependency_graph: None,
                group_metadata: None,
            },
            checks: StatusJsonChecks {
                project_metadata: "unavailable",
                group_coverage: "unavailable",
                resolution: "unavailable",
                sync: StatusJsonSync {
                    installed: None,
                    missing: None,
                    packages: None,
                },
            },
            messages,
        });
    };

    let mut drift = false;
    messages.push(format!(
        "lockfile: {} (packages={})",
        lock_path.display(),
        lock.packages.len()
    ));

    let project_metadata = if lock.metadata.minecraft != manifest.project.minecraft
        || lock.metadata.loader.kind != manifest.project.loader.kind
        || lock.metadata.loader.version != manifest.project.loader.version
    {
        drift = true;
        messages
            .push("project metadata: stale (minecraft/loader does not match manifest)".to_string());
        "stale"
    } else {
        messages.push("project metadata: aligned".to_string());
        "aligned"
    };

    let mut lock_usable_for_groups = true;
    let mut resolution = "unavailable";
    if !lock.metadata.dependency_graph {
        drift = true;
        lock_usable_for_groups = false;
        messages.push(format!(
            "resolution: lockfile does not contain dependency graph data; rerun `{lock_command}`"
        ));
    }

    let group_coverage =
        if let Err(err) = ensure_lock_group_metadata(&manifest, &lock, &active_groups) {
            drift = true;
            lock_usable_for_groups = false;
            messages.push(format!("group coverage: {err}"));
            "stale"
        } else if let Err(err) = ensure_lock_covers_groups(&manifest, &lock, &active_groups) {
            drift = true;
            lock_usable_for_groups = false;
            messages.push(format!("group coverage: {err}"));
            "stale"
        } else {
            messages.push("group coverage: ok".to_string());
            "ok"
        };

    let sync;
    if lock_usable_for_groups {
        let output = resolve_lockfile(
            &manifest,
            Some(&lock),
            &ResolveRequest {
                upgrade: false,
                groups: active_groups.clone(),
            },
        )?;
        let current_for_diff = filtered_lockfile(&lock, &active_groups);
        let entries = compute_lock_diff_entries(Some(&current_for_diff), &output.lockfile);
        if entries.is_empty() {
            resolution = "up_to_date";
            messages.push(format!(
                "resolution: up-to-date (install={} remove={} unchanged={})",
                output.plan.install.len(),
                output.plan.remove.len(),
                output.plan.unchanged.len()
            ));
        } else {
            drift = true;
            resolution = "stale";
            messages.push(format!(
                "resolution: stale (install={} remove={} unchanged={} changes={})",
                output.plan.install.len(),
                output.plan.remove.len(),
                output.plan.unchanged.len(),
                entries.len()
            ));
            messages.push(format!("next: run `{lock_command}`"));
        }

        let filtered = filtered_lockfile(&lock, &active_groups);
        let installed = filtered
            .packages
            .iter()
            .filter(|package| package_install_target_path(root, package).exists())
            .count();
        let missing = filtered.packages.len().saturating_sub(installed);
        if missing > 0 {
            drift = true;
        }
        messages.push(format!(
            "sync: installed={} missing={} packages={}",
            installed,
            missing,
            filtered.packages.len()
        ));
        if missing > 0 {
            messages.push(format!("next: run `{sync_command}`"));
        }
        sync = StatusJsonSync {
            installed: Some(installed),
            missing: Some(missing),
            packages: Some(filtered.packages.len()),
        };
    } else {
        messages.push("sync: unavailable until the lockfile is regenerated".to_string());
        messages.push(format!("next: run `{lock_command}`"));
        sync = StatusJsonSync {
            installed: None,
            missing: None,
            packages: None,
        };
    }

    Ok(StatusJsonReport {
        command: "status",
        groups,
        summary: StatusJsonSummary {
            state: if drift { "drift" } else { "clean" },
            exit_code: if drift { 2 } else { 0 },
        },
        manifest: StatusJsonManifest {
            exists: true,
            path: manifest_path.display().to_string(),
            roots: Some(selected_specs.len()),
            named_groups: Some(manifest.groups.0.len()),
        },
        lockfile: StatusJsonLockfile {
            exists: true,
            path: lock_path.display().to_string(),
            packages: Some(lock.packages.len()),
            dependency_graph: Some(lock.metadata.dependency_graph),
            group_metadata: Some(lock.metadata.group_metadata),
        },
        checks: StatusJsonChecks {
            project_metadata,
            group_coverage,
            resolution,
            sync,
        },
        messages,
    })
}

pub(crate) fn cmd_status(root: &Path, json: bool, selection: ProjectSelection<'_>) -> Result<()> {
    let groups = selection.groups.to_vec();
    let fallback_groups = selection.fallback_groups();
    if json {
        match build_status_json_report(
            root,
            groups.clone(),
            selection.all_groups,
            selection.profiles,
            selection.workspace,
        ) {
            Ok(report) => emit_json_report(&report, report.summary.exit_code),
            Err(err) => emit_json_report(
                &json_error_report("status", fallback_groups, format!("{err:#}"), 1),
                1,
            ),
        }
    } else {
        emit_command_report(render_status_json_report(&build_status_json_report(
            root,
            groups,
            selection.all_groups,
            selection.profiles,
            selection.workspace,
        )?))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use mineconda_core::{ModSide, ModSource, ModSpec, manifest_path, write_manifest};
    use mineconda_resolver::{ResolveRequest, resolve_lockfile};

    use super::*;
    use crate::command::lock::render_status_json_report;
    use crate::test_support::{TempProject, test_manifest};

    #[test]
    fn status_report_is_clean_for_synced_local_project() {
        let project = TempProject::new("status-clean");
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
                groups: BTreeSet::new(),
            },
        )
        .expect("resolve local lock");
        write_lockfile(&lockfile_path(&project.path), &output.lockfile).expect("write lock");
        fs::create_dir_all(project.path.join("mods")).expect("mods dir");
        let package = output.lockfile.packages.first().expect("locked package");
        fs::write(
            project.path.join(package.install_path_or_default()),
            b"installed",
        )
        .expect("installed package");

        let report = build_status_json_report(&project.path, Vec::new(), false, &[], None)
            .expect("status report");
        assert_eq!(report.summary.state, "clean");
        assert_eq!(report.summary.exit_code, 0);
        assert_eq!(report.checks.resolution, "up_to_date");
        assert_eq!(report.checks.sync.installed, Some(1));
        assert_eq!(report.checks.sync.missing, Some(0));
        let text = render_status_json_report(&report);
        assert!(text.output.contains("status summary: clean"));
        assert!(text.output.contains("resolution: up-to-date"));
        assert!(
            text.output
                .contains("sync: installed=1 missing=0 packages=1")
        );
    }

    #[test]
    fn status_report_marks_missing_lock_as_drift() {
        let project = TempProject::new("status-missing-lock");
        let manifest = test_manifest(vec![ModSpec::new(
            "demo".to_string(),
            ModSource::Local,
            "vendor/demo.jar".to_string(),
            ModSide::Both,
        )]);
        write_manifest(&manifest_path(&project.path), &manifest).expect("write manifest");

        let report = build_status_json_report(&project.path, Vec::new(), false, &[], None)
            .expect("status report");
        assert_eq!(report.summary.state, "drift");
        assert_eq!(report.summary.exit_code, 2);
        assert!(!report.lockfile.exists);
        assert_eq!(report.checks.resolution, "unavailable");
        let text = render_status_json_report(&report);
        assert!(text.output.contains("lockfile: missing"));
        assert!(text.output.contains("run `mineconda lock`"));
    }
}
