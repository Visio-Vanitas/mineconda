use std::{env, path::Path};

use anyhow::{Context, Result};
use mineconda_core::{ModSource, WorkspaceConfig};
use serde::Serialize;

use crate::build_lock_check_report;
use crate::build_lock_diff_json_report;
use crate::build_lock_write_report;
use crate::build_status_json_report;
use crate::build_sync_report;
use crate::cli::{ExportArg, ImportFormatArg, ImportSideArg, RunCommandArgs, SyncCommandArgs};
use crate::command::import_export::{
    build_export_json_report, build_export_report, build_import_json_report,
    build_workspace_member_import_report, resolve_workspace_member_import_archive,
    workspace_member_export_output,
};
use crate::command::run::{build_run_json_report, build_run_report, cmd_run};
use crate::command::sync::build_sync_json_report;
use crate::project::{
    activation_groups_with_profiles, load_manifest, load_workspace_required,
    normalized_profile_names, requested_groups_fallback, workspace_members,
};
use crate::report::{
    CommandReport, LockDiffEntry, LockDiffKind, emit_command_report, emit_json_report,
    render_lock_diff_entry, render_lock_write_report,
};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkspaceAggregateSummary {
    members: usize,
    changed: usize,
    failed: usize,
    exit_code: i32,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkspaceAggregateMemberJson {
    member: String,
    path: String,
    exit_code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkspaceAggregateJsonReport {
    command: &'static str,
    workspace: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    groups: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    profiles: Vec<String>,
    summary: WorkspaceAggregateSummary,
    members: Vec<WorkspaceAggregateMemberJson>,
}

#[derive(Debug, Clone)]
struct WorkspaceBatchMemberReport {
    member: String,
    output: String,
}

#[derive(Debug, Clone, Copy, Default)]
struct WorkspaceBatchCounts {
    ok: usize,
    stale_or_missing: usize,
    failed: usize,
}

fn record_workspace_batch_exit(counts: &mut WorkspaceBatchCounts, exit_code: i32) {
    match exit_code {
        0 => counts.ok += 1,
        2 => counts.stale_or_missing += 1,
        _ => counts.failed += 1,
    }
}

fn workspace_batch_exit_code(counts: WorkspaceBatchCounts) -> i32 {
    if counts.failed > 0 {
        1
    } else if counts.stale_or_missing > 0 {
        2
    } else {
        0
    }
}

fn render_workspace_batch_report(
    title: &str,
    members: &[WorkspaceBatchMemberReport],
    counts: WorkspaceBatchCounts,
) -> CommandReport {
    let mut lines = vec![format!("{title}: {} members", members.len())];
    for member in members {
        lines.push(format!("==> {}", member.member));
        let body = member.output.trim_end();
        if body.is_empty() {
            lines.push("(no output)".to_string());
        } else {
            lines.extend(body.lines().map(|line| line.to_string()));
        }
    }
    lines.push(format!(
        "workspace summary: ok={} stale={} failed={}",
        counts.ok, counts.stale_or_missing, counts.failed
    ));
    CommandReport {
        output: format!("{}\n", lines.join("\n")),
        exit_code: workspace_batch_exit_code(counts),
    }
}

fn resolve_workspace_root(root: &Path, action: &str) -> Result<std::path::PathBuf> {
    if root.is_absolute() {
        return Ok(root.to_path_buf());
    }

    Ok(env::current_dir()
        .with_context(|| format!("failed to read current working directory for {action}"))?
        .join(root))
}

fn build_workspace_aggregate_json_report(
    command: &'static str,
    workspace: String,
    groups: Vec<String>,
    profiles: Vec<String>,
    members: Vec<WorkspaceAggregateMemberJson>,
) -> WorkspaceAggregateJsonReport {
    let mut counts = WorkspaceBatchCounts::default();
    for member in &members {
        record_workspace_batch_exit(&mut counts, member.exit_code);
    }
    WorkspaceAggregateJsonReport {
        command,
        workspace,
        groups,
        profiles,
        summary: WorkspaceAggregateSummary {
            members: members.len(),
            changed: counts.stale_or_missing,
            failed: counts.failed,
            exit_code: workspace_batch_exit_code(counts),
        },
        members,
    }
}

fn workspace_member_json_error(
    member: &str,
    root: &Path,
    error: impl Into<String>,
    exit_code: i32,
) -> WorkspaceAggregateMemberJson {
    WorkspaceAggregateMemberJson {
        member: member.to_string(),
        path: root.display().to_string(),
        exit_code,
        report: None,
        error: Some(error.into()),
    }
}

fn workspace_member_json_report<T: Serialize>(
    member: &str,
    root: &Path,
    exit_code: i32,
    report: &T,
) -> Result<WorkspaceAggregateMemberJson> {
    Ok(WorkspaceAggregateMemberJson {
        member: member.to_string(),
        path: root.display().to_string(),
        exit_code,
        report: Some(serde_json::to_value(report)?),
        error: None,
    })
}

fn build_lock_member_report(
    root: &Path,
    upgrade: bool,
    check: bool,
    groups: Vec<String>,
    all_groups: bool,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<CommandReport> {
    let manifest = load_manifest(root)?;
    if check {
        return build_lock_check_report(
            root, &manifest, upgrade, groups, all_groups, profiles, workspace,
        );
    }

    let active_groups =
        activation_groups_with_profiles(&manifest, workspace, &groups, all_groups, profiles)?;
    Ok(render_lock_write_report(build_lock_write_report(
        root,
        &manifest,
        upgrade,
        active_groups,
    )?))
}

pub(crate) fn build_workspace_lock_report(
    root: &Path,
    upgrade: bool,
    check: bool,
    groups: Vec<String>,
    all_groups: bool,
    profiles: &[String],
) -> Result<CommandReport> {
    let workspace = load_workspace_required(root)?;
    let members = workspace_members(root, &workspace)?;
    let title = if check {
        "workspace lock check"
    } else {
        "workspace lock"
    };
    let mut counts = WorkspaceBatchCounts::default();
    let mut reports = Vec::new();

    for member in members {
        let report = match build_lock_member_report(
            &member.root,
            upgrade,
            check,
            groups.clone(),
            all_groups,
            profiles,
            Some(&workspace),
        ) {
            Ok(report) => report,
            Err(err) => CommandReport {
                output: format!("error: {err:#}\n"),
                exit_code: 1,
            },
        };
        record_workspace_batch_exit(&mut counts, report.exit_code);
        reports.push(WorkspaceBatchMemberReport {
            member: member.name,
            output: report.output,
        });
    }

    Ok(render_workspace_batch_report(title, &reports, counts))
}

pub(crate) fn cmd_lock_workspace(
    root: &Path,
    upgrade: bool,
    check: bool,
    groups: Vec<String>,
    all_groups: bool,
    profiles: &[String],
) -> Result<()> {
    emit_command_report(build_workspace_lock_report(
        root, upgrade, check, groups, all_groups, profiles,
    )?)
}

pub(crate) fn build_workspace_sync_report(
    root: &Path,
    args: SyncCommandArgs,
    profiles: &[String],
) -> Result<CommandReport> {
    let workspace = load_workspace_required(root)?;
    let members = workspace_members(root, &workspace)?;
    let title = if args.check {
        "workspace sync check"
    } else {
        "workspace sync"
    };
    let mut counts = WorkspaceBatchCounts::default();
    let mut reports = Vec::new();

    for member in members {
        let report = match build_sync_report(
            &member.root,
            SyncCommandArgs {
                prune: args.prune,
                check: args.check,
                locked: args.locked,
                offline: args.offline,
                jobs: args.jobs,
                verbose_cache: args.verbose_cache,
                groups: args.groups.clone(),
                all_groups: args.all_groups,
            },
            profiles,
            Some(&workspace),
        ) {
            Ok(report) => report,
            Err(err) => CommandReport {
                output: format!("error: {err:#}\n"),
                exit_code: 1,
            },
        };
        record_workspace_batch_exit(&mut counts, report.exit_code);
        reports.push(WorkspaceBatchMemberReport {
            member: member.name,
            output: report.output,
        });
    }

    Ok(render_workspace_batch_report(title, &reports, counts))
}

pub(crate) fn cmd_sync_workspace(
    root: &Path,
    args: SyncCommandArgs,
    profiles: &[String],
    json: bool,
) -> Result<()> {
    if json {
        let report = build_workspace_sync_json_report(root, args, profiles)?;
        return emit_json_report(&report, report.summary.exit_code);
    }
    emit_command_report(build_workspace_sync_report(root, args, profiles)?)
}

pub(crate) fn build_workspace_sync_json_report(
    root: &Path,
    args: SyncCommandArgs,
    profiles: &[String],
) -> Result<WorkspaceAggregateJsonReport> {
    let workspace = load_workspace_required(root)?;
    let members = workspace_members(root, &workspace)?;
    let fallback_groups = requested_groups_fallback(&args.groups, args.all_groups);
    let normalized_profiles = normalized_profile_names(profiles)?;
    let mut json_members = Vec::new();

    for member in members {
        match build_sync_json_report(&member.root, args.clone(), profiles, Some(&workspace)) {
            Ok(report) => json_members.push(workspace_member_json_report(
                &member.name,
                &member.root,
                report.summary.exit_code,
                &report,
            )?),
            Err(err) => json_members.push(workspace_member_json_error(
                &member.name,
                &member.root,
                format!("{err:#}"),
                1,
            )),
        }
    }

    Ok(build_workspace_aggregate_json_report(
        "sync",
        workspace.workspace.name,
        fallback_groups,
        normalized_profiles,
        json_members,
    ))
}

pub(crate) fn build_workspace_export_report(
    root: &Path,
    format: ExportArg,
    output: std::path::PathBuf,
    groups: Vec<String>,
    all_groups: bool,
    profiles: &[String],
) -> Result<CommandReport> {
    let workspace = load_workspace_required(root)?;
    let members = workspace_members(root, &workspace)?;
    let workspace_root = resolve_workspace_root(root, "workspace export")?;
    let base_output = if output.is_absolute() {
        output
    } else {
        workspace_root.join(output)
    };
    let mut counts = WorkspaceBatchCounts::default();
    let mut reports = Vec::new();

    for (index, member) in members.iter().enumerate() {
        let member_output =
            workspace_member_export_output(&base_output, &member.name, index + 1, members.len());
        let report = match build_export_report(
            &member.root,
            format,
            member_output,
            groups.clone(),
            all_groups,
            profiles,
            Some(&workspace),
        ) {
            Ok(report) => report,
            Err(err) => CommandReport {
                output: format!("error: {err:#}\n"),
                exit_code: 1,
            },
        };
        record_workspace_batch_exit(&mut counts, report.exit_code);
        reports.push(WorkspaceBatchMemberReport {
            member: member.name.clone(),
            output: report.output,
        });
    }

    Ok(render_workspace_batch_report(
        "workspace export",
        &reports,
        counts,
    ))
}

pub(crate) fn cmd_export_workspace(
    root: &Path,
    format: ExportArg,
    output: std::path::PathBuf,
    groups: Vec<String>,
    all_groups: bool,
    profiles: &[String],
    json: bool,
) -> Result<()> {
    if json {
        let report =
            build_workspace_export_json_report(root, format, output, groups, all_groups, profiles)?;
        return emit_json_report(&report, report.summary.exit_code);
    }
    emit_command_report(build_workspace_export_report(
        root, format, output, groups, all_groups, profiles,
    )?)
}

pub(crate) fn build_workspace_export_json_report(
    root: &Path,
    format: ExportArg,
    output: std::path::PathBuf,
    groups: Vec<String>,
    all_groups: bool,
    profiles: &[String],
) -> Result<WorkspaceAggregateJsonReport> {
    let workspace = load_workspace_required(root)?;
    let members = workspace_members(root, &workspace)?;
    let workspace_root = resolve_workspace_root(root, "workspace export")?;
    let base_output = if output.is_absolute() {
        output
    } else {
        workspace_root.join(output)
    };
    let fallback_groups = requested_groups_fallback(&groups, all_groups);
    let normalized_profiles = normalized_profile_names(profiles)?;
    let mut json_members = Vec::new();

    for (index, member) in members.iter().enumerate() {
        let member_output =
            workspace_member_export_output(&base_output, &member.name, index + 1, members.len());
        match build_export_json_report(
            &member.root,
            format,
            member_output,
            groups.clone(),
            all_groups,
            profiles,
            Some(&workspace),
        ) {
            Ok(report) => json_members.push(workspace_member_json_report(
                &member.name,
                &member.root,
                report.summary.exit_code,
                &report,
            )?),
            Err(err) => json_members.push(workspace_member_json_error(
                &member.name,
                &member.root,
                format!("{err:#}"),
                1,
            )),
        }
    }

    Ok(build_workspace_aggregate_json_report(
        "export",
        workspace.workspace.name,
        fallback_groups,
        normalized_profiles,
        json_members,
    ))
}

pub(crate) fn build_workspace_run_report(
    root: &Path,
    args: RunCommandArgs,
    profiles: &[String],
) -> Result<CommandReport> {
    let workspace = load_workspace_required(root)?;
    let members = workspace_members(root, &workspace)?;
    let mut counts = WorkspaceBatchCounts::default();
    let mut reports = Vec::new();

    for member in members {
        let report = match build_run_report(&member.root, args.clone(), profiles, Some(&workspace))
        {
            Ok(report) => report,
            Err(err) => CommandReport {
                output: format!("error: {err:#}\n"),
                exit_code: 1,
            },
        };
        record_workspace_batch_exit(&mut counts, report.exit_code);
        reports.push(WorkspaceBatchMemberReport {
            member: member.name,
            output: report.output,
        });
    }

    Ok(render_workspace_batch_report(
        "workspace run",
        &reports,
        counts,
    ))
}

pub(crate) fn cmd_run_workspace(
    root: &Path,
    args: RunCommandArgs,
    profiles: &[String],
    json: bool,
) -> Result<()> {
    if json {
        let report = build_workspace_run_json_report(root, args, profiles)?;
        return emit_json_report(&report, report.summary.exit_code);
    }
    if args.dry_run {
        return emit_command_report(build_workspace_run_report(root, args, profiles)?);
    }

    let workspace = load_workspace_required(root)?;
    let members = workspace_members(root, &workspace)?;
    let mut counts = WorkspaceBatchCounts::default();

    println!("workspace run: {} members", members.len());
    for member in members {
        println!("==> {}", member.name);
        match cmd_run(&member.root, args.clone(), profiles, Some(&workspace)) {
            Ok(()) => record_workspace_batch_exit(&mut counts, 0),
            Err(err) => {
                record_workspace_batch_exit(&mut counts, 1);
                eprintln!("error: {err:#}");
            }
        }
    }

    emit_command_report(CommandReport {
        output: format!(
            "workspace summary: ok={} stale={} failed={}\n",
            counts.ok, counts.stale_or_missing, counts.failed
        ),
        exit_code: workspace_batch_exit_code(counts),
    })
}

pub(crate) fn build_workspace_run_json_report(
    root: &Path,
    args: RunCommandArgs,
    profiles: &[String],
) -> Result<WorkspaceAggregateJsonReport> {
    let workspace = load_workspace_required(root)?;
    let members = workspace_members(root, &workspace)?;
    let fallback_groups = requested_groups_fallback(&args.groups, args.all_groups);
    let normalized_profiles = normalized_profile_names(profiles)?;
    let mut json_members = Vec::new();

    for member in members {
        let run_report =
            build_run_json_report(&member.root, args.clone(), profiles, Some(&workspace));
        if args.dry_run {
            match run_report {
                Ok(report) => json_members.push(workspace_member_json_report(
                    &member.name,
                    &member.root,
                    report.summary.exit_code,
                    &report,
                )?),
                Err(err) => json_members.push(workspace_member_json_error(
                    &member.name,
                    &member.root,
                    format!("{err:#}"),
                    1,
                )),
            }
            continue;
        }

        match run_report {
            Ok(report) => match cmd_run(&member.root, args.clone(), profiles, Some(&workspace)) {
                Ok(()) => json_members.push(workspace_member_json_report(
                    &member.name,
                    &member.root,
                    0,
                    &report,
                )?),
                Err(err) => json_members.push(workspace_member_json_error(
                    &member.name,
                    &member.root,
                    format!("{err:#}"),
                    1,
                )),
            },
            Err(err) => json_members.push(workspace_member_json_error(
                &member.name,
                &member.root,
                format!("{err:#}"),
                1,
            )),
        }
    }

    Ok(build_workspace_aggregate_json_report(
        "run",
        workspace.workspace.name,
        fallback_groups,
        normalized_profiles,
        json_members,
    ))
}

pub(crate) fn build_workspace_import_report(
    root: &Path,
    input: String,
    format: ImportFormatArg,
    side: ImportSideArg,
    force: bool,
) -> Result<CommandReport> {
    let workspace = load_workspace_required(root)?;
    let members = workspace_members(root, &workspace)?;
    let workspace_root = resolve_workspace_root(root, "workspace import")?;
    let base_input = if Path::new(&input).is_absolute() {
        std::path::PathBuf::from(&input)
    } else {
        workspace_root.join(&input)
    };
    let mut counts = WorkspaceBatchCounts::default();
    let mut reports = Vec::new();

    for member in members {
        let report = build_workspace_member_import_report(
            &member.root,
            &base_input,
            &member.name,
            format,
            side,
            force,
        )
        .unwrap_or_else(|err| CommandReport {
            output: format!("error: {err:#}\n"),
            exit_code: 1,
        });
        record_workspace_batch_exit(&mut counts, report.exit_code);
        reports.push(WorkspaceBatchMemberReport {
            member: member.name,
            output: report.output,
        });
    }

    Ok(render_workspace_batch_report(
        "workspace import",
        &reports,
        counts,
    ))
}

pub(crate) fn build_workspace_import_json_report(
    root: &Path,
    input: String,
    format: ImportFormatArg,
    side: ImportSideArg,
    force: bool,
) -> Result<WorkspaceAggregateJsonReport> {
    let workspace = load_workspace_required(root)?;
    let members = workspace_members(root, &workspace)?;
    let workspace_root = resolve_workspace_root(root, "workspace import")?;
    let base_input = if Path::new(&input).is_absolute() {
        std::path::PathBuf::from(&input)
    } else {
        workspace_root.join(&input)
    };
    let mut json_members = Vec::new();

    for member in members {
        match resolve_workspace_member_import_archive(&base_input, &member.name) {
            Ok(archive) => match build_import_json_report(
                &member.root,
                archive.display().to_string(),
                format,
                side,
                force,
            ) {
                Ok(report) => json_members.push(workspace_member_json_report(
                    &member.name,
                    &member.root,
                    report.summary.exit_code,
                    &report,
                )?),
                Err(err) => json_members.push(workspace_member_json_error(
                    &member.name,
                    &member.root,
                    format!("{err:#}"),
                    1,
                )),
            },
            Err(err) => json_members.push(workspace_member_json_error(
                &member.name,
                &member.root,
                format!("{err:#}"),
                1,
            )),
        }
    }

    Ok(build_workspace_aggregate_json_report(
        "import",
        workspace.workspace.name,
        Vec::new(),
        Vec::new(),
        json_members,
    ))
}

pub(crate) fn cmd_import_workspace(
    root: &Path,
    input: String,
    format: ImportFormatArg,
    side: ImportSideArg,
    force: bool,
    json: bool,
) -> Result<()> {
    if json {
        let report = build_workspace_import_json_report(root, input, format, side, force)?;
        return emit_json_report(&report, report.summary.exit_code);
    }
    emit_command_report(build_workspace_import_report(
        root, input, format, side, force,
    )?)
}

pub(crate) fn cmd_status_workspace(
    root: &Path,
    groups: Vec<String>,
    all_groups: bool,
    profiles: &[String],
    json: bool,
) -> Result<()> {
    let workspace = load_workspace_required(root)?;
    let members = workspace_members(root, &workspace)?;
    let fallback_groups = requested_groups_fallback(&groups, all_groups);
    let normalized_profiles = normalized_profile_names(profiles)?;
    let mut changed = 0usize;
    let mut failed = 0usize;
    let mut json_members = Vec::new();
    let mut lines = vec![format!("workspace status: {} members", members.len())];

    for member in members {
        match build_status_json_report(
            &member.root,
            groups.clone(),
            all_groups,
            profiles,
            Some(&workspace),
        ) {
            Ok(report) => {
                if report.summary.exit_code != 0 {
                    changed += 1;
                }
                lines.push(format!("{}: {}", member.name, report.summary.state));
                json_members.push(WorkspaceAggregateMemberJson {
                    member: member.name.clone(),
                    path: member.root.display().to_string(),
                    exit_code: report.summary.exit_code,
                    report: Some(serde_json::to_value(&report)?),
                    error: None,
                });
            }
            Err(err) => {
                failed += 1;
                lines.push(format!("{}: error: {err}", member.name));
                json_members.push(WorkspaceAggregateMemberJson {
                    member: member.name.clone(),
                    path: member.root.display().to_string(),
                    exit_code: 1,
                    report: None,
                    error: Some(format!("{err:#}")),
                });
            }
        }
    }

    let exit_code = if failed > 0 {
        1
    } else if changed > 0 {
        2
    } else {
        0
    };
    if json {
        return emit_json_report(
            &WorkspaceAggregateJsonReport {
                command: "status",
                workspace: workspace.workspace.name,
                groups: fallback_groups,
                profiles: normalized_profiles,
                summary: WorkspaceAggregateSummary {
                    members: json_members.len(),
                    changed,
                    failed,
                    exit_code,
                },
                members: json_members,
            },
            exit_code,
        );
    }

    emit_command_report(CommandReport {
        output: format!("{}\n", lines.join("\n")),
        exit_code,
    })
}

pub(crate) fn cmd_lock_diff_workspace(
    root: &Path,
    upgrade: bool,
    groups: Vec<String>,
    all_groups: bool,
    profiles: &[String],
    json: bool,
) -> Result<()> {
    let workspace = load_workspace_required(root)?;
    let members = workspace_members(root, &workspace)?;
    let fallback_groups = requested_groups_fallback(&groups, all_groups);
    let normalized_profiles = normalized_profile_names(profiles)?;
    let mut changed = 0usize;
    let mut failed = 0usize;
    let mut json_members = Vec::new();
    let mut lines = vec![format!("workspace lock diff: {} members", members.len())];

    for member in members {
        match build_lock_diff_json_report(
            &member.root,
            upgrade,
            groups.clone(),
            all_groups,
            profiles,
            Some(&workspace),
        ) {
            Ok(Ok(report)) => {
                if !report.entries.is_empty() {
                    changed += 1;
                }
                lines.push(format!("{}: {} changes", member.name, report.entries.len()));
                for entry in &report.entries {
                    lines.push(format!(
                        "  [{}] {}",
                        member.name,
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
                            current_dependencies: Vec::new(),
                            desired_dependencies: Vec::new(),
                            current_artifact: entry.current_artifact.clone(),
                            desired_artifact: entry.desired_artifact.clone(),
                        })
                    ));
                }
                json_members.push(WorkspaceAggregateMemberJson {
                    member: member.name.clone(),
                    path: member.root.display().to_string(),
                    exit_code: if report.entries.is_empty() { 0 } else { 2 },
                    report: Some(serde_json::to_value(&report)?),
                    error: None,
                });
            }
            Ok(Err(error)) => {
                failed += 1;
                lines.push(format!("{}: {}", member.name, error.error));
                json_members.push(WorkspaceAggregateMemberJson {
                    member: member.name.clone(),
                    path: member.root.display().to_string(),
                    exit_code: error.exit_code,
                    report: Some(serde_json::to_value(&error)?),
                    error: Some(error.error),
                });
            }
            Err(err) => {
                failed += 1;
                lines.push(format!("{}: error: {err}", member.name));
                json_members.push(WorkspaceAggregateMemberJson {
                    member: member.name.clone(),
                    path: member.root.display().to_string(),
                    exit_code: 1,
                    report: None,
                    error: Some(format!("{err:#}")),
                });
            }
        }
    }

    let exit_code = if failed > 0 {
        1
    } else if changed > 0 {
        2
    } else {
        0
    };
    if json {
        return emit_json_report(
            &WorkspaceAggregateJsonReport {
                command: "lock-diff",
                workspace: workspace.workspace.name,
                groups: fallback_groups,
                profiles: normalized_profiles,
                summary: WorkspaceAggregateSummary {
                    members: json_members.len(),
                    changed,
                    failed,
                    exit_code,
                },
                members: json_members,
            },
            exit_code,
        );
    }

    emit_command_report(CommandReport {
        output: format!("{}\n", lines.join("\n")),
        exit_code,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use mineconda_core::{
        DEFAULT_GROUP_NAME, HashAlgorithm, LockedPackage, ModSide, ModSource, ModSpec, PackageHash,
        lockfile_path, manifest_path, write_manifest,
    };
    use mineconda_export::{ExportFormat, ExportRequest, export_pack};
    use serde_json::Value;

    use super::*;
    use crate::cli::{ImportFormatArg, ImportSideArg, RunModeArg};
    use crate::test_support::{
        TempProject, install_locked_packages, test_lockfile, test_manifest,
        write_local_member_manifest, write_lock_for_manifest, write_workspace_fixture,
    };
    use crate::{load_lockfile_required, load_manifest};

    fn write_pinned_local_member_manifest(root: &Path, id: &str) -> mineconda_core::Manifest {
        let mut manifest = write_local_member_manifest(root, id);
        manifest.project.loader.version = "21.1.227".to_string();
        write_manifest(&manifest_path(root), &manifest).expect("rewrite manifest");
        load_manifest(root).expect("reload pinned manifest")
    }

    fn write_client_launcher(root: &Path) {
        let dev_root = root.join(".mineconda/dev");
        fs::create_dir_all(&dev_root).expect("dev root");
        fs::write(dev_root.join("neoforge-client-launch.jar"), b"launcher").expect("launcher");
    }

    fn write_test_mrpack(dir: &Path, file_name: &str) -> std::path::PathBuf {
        let manifest = test_manifest(vec![ModSpec::new(
            "jei".to_string(),
            ModSource::Modrinth,
            "latest".to_string(),
            ModSide::Both,
        )]);
        let lockfile = test_lockfile(vec![LockedPackage {
            id: "jei".to_string(),
            source: ModSource::Modrinth,
            version: "1.0.0".to_string(),
            side: ModSide::Both,
            file_name: "jei.jar".to_string(),
            install_path: None,
            file_size: Some(1),
            sha256: "b".repeat(64),
            download_url: "https://example.invalid/jei.jar".to_string(),
            hashes: vec![
                PackageHash {
                    algorithm: HashAlgorithm::Sha1,
                    value: "a".repeat(40),
                },
                PackageHash {
                    algorithm: HashAlgorithm::Sha256,
                    value: "b".repeat(64),
                },
                PackageHash {
                    algorithm: HashAlgorithm::Sha512,
                    value: "c".repeat(128),
                },
            ],
            source_ref: Some("requested=jei;project=jei;version=1.0.0;name=jei".to_string()),
            groups: vec![DEFAULT_GROUP_NAME.to_string()],
            dependencies: Vec::new(),
        }]);
        let output = dir.join(file_name);
        export_pack(
            &manifest,
            &lockfile,
            &ExportRequest {
                output: output.clone(),
                format: ExportFormat::Mrpack,
                project_root: None,
            },
        )
        .expect("write test mrpack");
        output
    }

    #[test]
    fn workspace_lock_report_writes_all_members() {
        let project = TempProject::new("workspace-lock");
        write_workspace_fixture(&project.path, &["packs/client", "packs/server"]);
        let client_root = project.path.join("packs/client");
        let server_root = project.path.join("packs/server");
        let client_manifest = write_local_member_manifest(&client_root, "client-demo");
        let server_manifest = write_local_member_manifest(&server_root, "server-demo");

        let report =
            build_workspace_lock_report(&project.path, false, false, Vec::new(), false, &[])
                .expect("workspace lock report");

        assert_eq!(report.exit_code, 0);
        assert!(report.output.contains("workspace lock: 2 members"));
        assert!(report.output.contains("==> packs/client"));
        assert!(report.output.contains("==> packs/server"));
        assert!(lockfile_path(&client_root).exists());
        assert!(lockfile_path(&server_root).exists());

        let client_lock = load_lockfile_required(&client_root).expect("client lock");
        let server_lock = load_lockfile_required(&server_root).expect("server lock");
        assert_eq!(client_lock.packages.len(), 1);
        assert_eq!(server_lock.packages.len(), 1);
        assert_eq!(client_manifest.mods.len(), 1);
        assert_eq!(server_manifest.mods.len(), 1);
    }

    #[test]
    fn workspace_lock_check_report_prioritizes_failures_over_stale() {
        let project = TempProject::new("workspace-lock-check");
        write_workspace_fixture(
            &project.path,
            &["packs/clean", "packs/stale", "packs/broken"],
        );

        let clean_root = project.path.join("packs/clean");
        let clean_manifest = write_local_member_manifest(&clean_root, "clean-demo");
        write_lock_for_manifest(&clean_root, &clean_manifest);

        let stale_root = project.path.join("packs/stale");
        let stale_manifest = write_local_member_manifest(&stale_root, "stale-demo");
        write_lock_for_manifest(&stale_root, &stale_manifest);
        fs::write(stale_root.join("vendor/stale-extra.jar"), b"extra").expect("extra jar");
        let updated_manifest = test_manifest(vec![
            ModSpec::new(
                "stale-demo".to_string(),
                ModSource::Local,
                "vendor/stale-demo.jar".to_string(),
                ModSide::Both,
            ),
            ModSpec::new(
                "stale-extra".to_string(),
                ModSource::Local,
                "vendor/stale-extra.jar".to_string(),
                ModSide::Both,
            ),
        ]);
        write_manifest(&manifest_path(&stale_root), &updated_manifest).expect("rewrite manifest");

        let broken_root = project.path.join("packs/broken");
        fs::create_dir_all(&broken_root).expect("broken dir");

        let report =
            build_workspace_lock_report(&project.path, false, true, Vec::new(), false, &[])
                .expect("workspace lock check report");

        assert_eq!(report.exit_code, 1);
        assert!(report.output.contains("workspace lock check: 3 members"));
        assert!(report.output.contains("lock check: up-to-date"));
        assert!(report.output.contains("lock check: stale"));
        assert!(report.output.contains("ADD stale-extra [local]"));
        assert!(report.output.contains("==> packs/broken"));
        assert!(report.output.contains("error:"));
        assert!(
            report
                .output
                .contains("workspace summary: ok=1 stale=1 failed=1")
        );
    }

    #[test]
    fn workspace_sync_check_report_tracks_ok_missing_and_failed_members() {
        let project = TempProject::new("workspace-sync-check");
        write_workspace_fixture(
            &project.path,
            &["packs/clean", "packs/missing", "packs/unlocked"],
        );

        let clean_root = project.path.join("packs/clean");
        let clean_manifest = write_local_member_manifest(&clean_root, "clean-demo");
        let clean_lock = write_lock_for_manifest(&clean_root, &clean_manifest);
        install_locked_packages(&clean_root, &clean_lock);

        let missing_root = project.path.join("packs/missing");
        let missing_manifest = write_local_member_manifest(&missing_root, "missing-demo");
        write_lock_for_manifest(&missing_root, &missing_manifest);

        let unlocked_root = project.path.join("packs/unlocked");
        write_local_member_manifest(&unlocked_root, "unlocked-demo");

        let report = build_workspace_sync_report(
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
        )
        .expect("workspace sync check report");

        assert_eq!(report.exit_code, 1);
        assert!(report.output.contains("workspace sync check: 3 members"));
        assert!(report.output.contains("sync check: installed"));
        assert!(report.output.contains("sync check: missing"));
        assert!(
            report
                .output
                .contains("lockfile missing; run `mineconda lock` first")
        );
        assert!(
            report
                .output
                .contains("workspace summary: ok=1 stale=1 failed=1")
        );
    }

    #[test]
    fn workspace_sync_report_installs_all_members() {
        let project = TempProject::new("workspace-sync");
        write_workspace_fixture(&project.path, &["packs/client", "packs/server"]);

        let client_root = project.path.join("packs/client");
        let client_manifest = write_local_member_manifest(&client_root, "client-demo");
        write_lock_for_manifest(&client_root, &client_manifest);

        let server_root = project.path.join("packs/server");
        let server_manifest = write_local_member_manifest(&server_root, "server-demo");
        write_lock_for_manifest(&server_root, &server_manifest);

        let report = build_workspace_sync_report(
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
        )
        .expect("workspace sync report");

        assert_eq!(report.exit_code, 0);
        assert!(report.output.contains("workspace sync: 2 members"));
        assert!(report.output.contains("==> packs/client"));
        assert!(report.output.contains("==> packs/server"));
        assert!(report.output.contains("sync done: packages=1"));
        assert!(client_root.join("mods/client-demo.jar").exists());
        assert!(server_root.join("mods/server-demo.jar").exists());
        assert!(
            report
                .output
                .contains("workspace summary: ok=2 stale=0 failed=0")
        );
    }

    #[test]
    fn workspace_export_report_writes_distinct_member_artifacts() {
        let project = TempProject::new("workspace-export");
        write_workspace_fixture(&project.path, &["packs/client", "packs/server"]);

        let client_root = project.path.join("packs/client");
        let client_manifest = write_pinned_local_member_manifest(&client_root, "client-demo");
        write_lock_for_manifest(&client_root, &client_manifest);

        let server_root = project.path.join("packs/server");
        let server_manifest = write_pinned_local_member_manifest(&server_root, "server-demo");
        write_lock_for_manifest(&server_root, &server_manifest);

        let report = build_workspace_export_report(
            &project.path,
            ExportArg::ModsDesc,
            "dist/modpack".into(),
            Vec::new(),
            false,
            &[],
        )
        .expect("workspace export report");

        let client_export = project.path.join("dist/modpack-1-packs-client.json");
        let server_export = project.path.join("dist/modpack-2-packs-server.json");

        assert_eq!(report.exit_code, 0);
        assert!(report.output.contains("workspace export: 2 members"));
        assert!(
            report
                .output
                .contains(client_export.display().to_string().as_str())
        );
        assert!(
            report
                .output
                .contains(server_export.display().to_string().as_str())
        );
        assert!(client_export.exists());
        assert!(server_export.exists());
        assert!(
            report
                .output
                .contains("workspace summary: ok=2 stale=0 failed=0")
        );
    }

    #[test]
    fn workspace_export_report_records_member_failures() {
        let project = TempProject::new("workspace-export-fail");
        write_workspace_fixture(&project.path, &["packs/client", "packs/unlocked"]);

        let client_root = project.path.join("packs/client");
        let client_manifest = write_pinned_local_member_manifest(&client_root, "client-demo");
        write_lock_for_manifest(&client_root, &client_manifest);

        let unlocked_root = project.path.join("packs/unlocked");
        write_pinned_local_member_manifest(&unlocked_root, "unlocked-demo");

        let report = build_workspace_export_report(
            &project.path,
            ExportArg::ModsDesc,
            "dist/modpack".into(),
            Vec::new(),
            false,
            &[],
        )
        .expect("workspace export report");

        assert_eq!(report.exit_code, 1);
        assert!(report.output.contains("workspace export: 2 members"));
        assert!(report.output.contains("==> packs/unlocked"));
        assert!(
            report
                .output
                .contains("error: lockfile not found, run `mineconda lock` first")
        );
        assert!(
            report
                .output
                .contains("workspace summary: ok=1 stale=0 failed=1")
        );
    }

    #[test]
    fn workspace_export_report_resolves_relative_output_from_workspace_root() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let root = std::path::PathBuf::from(format!(
            "target/workspace-export-relative-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("workspace dir");
        write_workspace_fixture(&root, &["packs/client", "packs/server"]);

        let client_root = root.join("packs/client");
        let client_manifest = write_pinned_local_member_manifest(&client_root, "client-demo");
        write_lock_for_manifest(&client_root, &client_manifest);

        let server_root = root.join("packs/server");
        let server_manifest = write_pinned_local_member_manifest(&server_root, "server-demo");
        write_lock_for_manifest(&server_root, &server_manifest);

        let report = build_workspace_export_report(
            &root,
            ExportArg::ModsDesc,
            "dist/modpack".into(),
            Vec::new(),
            false,
            &[],
        )
        .expect("workspace export report");

        assert_eq!(report.exit_code, 0);
        assert!(root.join("dist/modpack-1-packs-client.json").exists());
        assert!(root.join("dist/modpack-2-packs-server.json").exists());
        assert!(
            !client_root
                .join("dist/modpack-1-packs-client.json")
                .exists()
        );
        assert!(
            !server_root
                .join("dist/modpack-2-packs-server.json")
                .exists()
        );

        fs::remove_dir_all(&root).expect("cleanup relative workspace fixture");
    }

    #[test]
    fn workspace_run_report_renders_all_members_dry_run() {
        let project = TempProject::new("workspace-run");
        write_workspace_fixture(&project.path, &["packs/client", "packs/server"]);

        let client_root = project.path.join("packs/client");
        write_local_member_manifest(&client_root, "client-demo");
        write_client_launcher(&client_root);

        let server_root = project.path.join("packs/server");
        write_local_member_manifest(&server_root, "server-demo");
        write_client_launcher(&server_root);

        let report = build_workspace_run_report(
            &project.path,
            RunCommandArgs {
                dry_run: true,
                java: None,
                memory: None,
                jvm_args: Vec::new(),
                mode: RunModeArg::Client,
                username: "DevPlayer".to_string(),
                instance: "dev".to_string(),
                launcher_jar: None,
                server_jar: None,
                groups: Vec::new(),
                all_groups: false,
            },
            &[],
        )
        .expect("workspace run report");

        assert_eq!(report.exit_code, 0);
        assert!(report.output.contains("workspace run: 2 members"));
        assert!(report.output.contains("==> packs/client"));
        assert!(report.output.contains("==> packs/server"));
        assert!(report.output.contains("dry-run [client]:"));
        assert!(
            report
                .output
                .contains("workspace summary: ok=2 stale=0 failed=0")
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_run_executes_all_members_sequentially() {
        let project = TempProject::new("workspace-run-exec");
        write_workspace_fixture(&project.path, &["packs/client", "packs/server"]);

        let client_root = project.path.join("packs/client");
        write_local_member_manifest(&client_root, "client-demo");
        write_client_launcher(&client_root);

        let server_root = project.path.join("packs/server");
        write_local_member_manifest(&server_root, "server-demo");
        write_client_launcher(&server_root);

        cmd_run_workspace(
            &project.path,
            RunCommandArgs {
                dry_run: false,
                java: Some("/usr/bin/true".to_string()),
                memory: None,
                jvm_args: Vec::new(),
                mode: RunModeArg::Client,
                username: "DevPlayer".to_string(),
                instance: "dev".to_string(),
                launcher_jar: None,
                server_jar: None,
                groups: Vec::new(),
                all_groups: false,
            },
            &[],
            false,
        )
        .expect("workspace run should succeed");
    }

    #[test]
    fn workspace_import_json_report_collects_member_results() {
        let workspace_root = TempProject::new("workspace-import-json");
        write_workspace_fixture(&workspace_root.path, &["packs/client", "packs/server"]);

        let client_dir = workspace_root.path.join("imports/packs/client");
        fs::create_dir_all(&client_dir).expect("client import dir");
        write_test_mrpack(&client_dir, "client-pack.mrpack");

        let server_dir = workspace_root.path.join("imports/packs/server");
        fs::create_dir_all(&server_dir).expect("server import dir");
        write_test_mrpack(&server_dir, "server-pack.mrpack");

        let report = build_workspace_import_json_report(
            &workspace_root.path,
            "imports".to_string(),
            ImportFormatArg::Auto,
            ImportSideArg::Client,
            false,
        )
        .expect("workspace import json report");

        assert_eq!(report.summary.exit_code, 0);
        assert_eq!(report.summary.members, 2);
        assert_eq!(report.summary.failed, 0);
        assert_eq!(report.members.len(), 2);
        for member in &report.members {
            assert_eq!(member.exit_code, 0);
            let value: Value = member.report.clone().expect("member report");
            assert_eq!(value["command"], "import");
            assert_eq!(value["detected_format"], "modrinth-mrpack");
        }
        assert!(
            workspace_root
                .path
                .join("packs/client/mineconda.toml")
                .exists()
        );
        assert!(
            workspace_root
                .path
                .join("packs/server/mineconda.toml")
                .exists()
        );
    }
}
