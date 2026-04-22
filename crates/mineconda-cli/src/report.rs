use std::io::Write;
use std::process;

use anyhow::{Context, Result};
use mineconda_core::{LockedDependency, ModSource};
use serde::Serialize;

use crate::project::format_group_list;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LockDiffKind {
    Add,
    Remove,
    Upgrade,
    Downgrade,
    ChangeVersion,
    ChangeArtifact,
    ChangeGroups,
    ChangeDependencies,
}

impl LockDiffKind {
    fn label(self) -> &'static str {
        match self {
            Self::Add => "ADD",
            Self::Remove => "REMOVE",
            Self::Upgrade => "UPGRADE",
            Self::Downgrade => "DOWNGRADE",
            Self::ChangeVersion => "CHANGE VERSION",
            Self::ChangeArtifact => "CHANGE ARTIFACT",
            Self::ChangeGroups => "CHANGE GROUPS",
            Self::ChangeDependencies => "CHANGE DEPENDENCIES",
        }
    }

    fn json_label(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
            Self::Upgrade => "upgrade",
            Self::Downgrade => "downgrade",
            Self::ChangeVersion => "change_version",
            Self::ChangeArtifact => "change_artifact",
            Self::ChangeGroups => "change_groups",
            Self::ChangeDependencies => "change_dependencies",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LockDiffEntry {
    pub(crate) kind: LockDiffKind,
    pub(crate) id: String,
    pub(crate) source: ModSource,
    pub(crate) current_version: Option<String>,
    pub(crate) desired_version: Option<String>,
    pub(crate) current_groups: Vec<String>,
    pub(crate) desired_groups: Vec<String>,
    pub(crate) current_dependencies: Vec<LockedDependency>,
    pub(crate) desired_dependencies: Vec<LockedDependency>,
    pub(crate) current_artifact: Option<String>,
    pub(crate) desired_artifact: Option<String>,
}

#[derive(Debug)]
pub(crate) struct CommandReport {
    pub(crate) output: String,
    pub(crate) exit_code: i32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LockWriteReport {
    pub(crate) install: usize,
    pub(crate) remove: usize,
    pub(crate) unchanged: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct JsonErrorReport {
    pub(crate) command: &'static str,
    pub(crate) groups: Vec<String>,
    pub(crate) error: String,
    pub(crate) exit_code: i32,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LockDiffJsonSummary {
    pub(crate) install: usize,
    pub(crate) remove: usize,
    pub(crate) unchanged: usize,
    pub(crate) changes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LockDependencyJson {
    pub(crate) source: String,
    pub(crate) id: String,
    pub(crate) kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) constraint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LockDiffJsonEntry {
    pub(crate) kind: String,
    pub(crate) id: String,
    pub(crate) source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) current_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) desired_version: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) current_groups: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) desired_groups: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) current_dependencies: Vec<LockDependencyJson>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) desired_dependencies: Vec<LockDependencyJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) current_artifact: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) desired_artifact: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LockDiffJsonReport {
    pub(crate) command: &'static str,
    pub(crate) groups: Vec<String>,
    pub(crate) summary: LockDiffJsonSummary,
    pub(crate) entries: Vec<LockDiffJsonEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusJsonSummary {
    pub(crate) state: &'static str,
    pub(crate) exit_code: i32,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusJsonManifest {
    pub(crate) exists: bool,
    pub(crate) path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) roots: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) named_groups: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusJsonLockfile {
    pub(crate) exists: bool,
    pub(crate) path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) packages: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) dependency_graph: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) group_metadata: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusJsonSync {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) installed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) missing: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) packages: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusJsonChecks {
    pub(crate) project_metadata: &'static str,
    pub(crate) group_coverage: &'static str,
    pub(crate) resolution: &'static str,
    pub(crate) sync: StatusJsonSync,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusJsonReport {
    pub(crate) command: &'static str,
    pub(crate) groups: Vec<String>,
    pub(crate) summary: StatusJsonSummary,
    pub(crate) manifest: StatusJsonManifest,
    pub(crate) lockfile: StatusJsonLockfile,
    pub(crate) checks: StatusJsonChecks,
    pub(crate) messages: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RunJsonSummary {
    pub(crate) exit_code: i32,
    pub(crate) launches: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RunJsonLaunch {
    pub(crate) role: String,
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RunJsonReport {
    pub(crate) command: &'static str,
    pub(crate) groups: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) profiles: Vec<String>,
    pub(crate) mode: String,
    pub(crate) instance: String,
    pub(crate) dry_run: bool,
    pub(crate) java: String,
    pub(crate) memory: String,
    pub(crate) summary: RunJsonSummary,
    pub(crate) launches: Vec<RunJsonLaunch>,
    pub(crate) messages: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ExportJsonSummary {
    pub(crate) exit_code: i32,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ExportJsonReport {
    pub(crate) command: &'static str,
    pub(crate) groups: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) profiles: Vec<String>,
    pub(crate) format: String,
    pub(crate) output: String,
    pub(crate) summary: ExportJsonSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) compatibility_warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) resolved_loader_version: Option<String>,
    pub(crate) messages: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ImportJsonSummary {
    pub(crate) exit_code: i32,
    pub(crate) mods: usize,
    pub(crate) packages: usize,
    pub(crate) overrides: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ImportJsonReport {
    pub(crate) command: &'static str,
    pub(crate) input: String,
    pub(crate) detected_format: String,
    pub(crate) side: String,
    pub(crate) force: bool,
    pub(crate) summary: ImportJsonSummary,
    pub(crate) manifest_path: String,
    pub(crate) lockfile_path: String,
    pub(crate) messages: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SyncJsonSummary {
    pub(crate) mode: &'static str,
    pub(crate) state: &'static str,
    pub(crate) exit_code: i32,
    pub(crate) packages: usize,
    pub(crate) installed: usize,
    pub(crate) missing: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) local_hits: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) s3_hits: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) origin_downloads: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) network_attempts: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) removed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) failed: Option<usize>,
    pub(crate) lockfile_updated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SyncJsonMissingPackage {
    pub(crate) id: String,
    pub(crate) source: String,
    pub(crate) version: String,
    pub(crate) target: String,
    pub(crate) groups: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SyncJsonReport {
    pub(crate) command: &'static str,
    pub(crate) groups: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) profiles: Vec<String>,
    pub(crate) summary: SyncJsonSummary,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) missing_packages: Vec<SyncJsonMissingPackage>,
    pub(crate) messages: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LsJsonSummary {
    pub(crate) roots: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LsJsonItem {
    pub(crate) group: String,
    pub(crate) id: String,
    pub(crate) source: String,
    pub(crate) requested_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) locked_version: Option<String>,
    pub(crate) status: String,
    pub(crate) install_path: String,
    pub(crate) side: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LsJsonReport {
    pub(crate) command: &'static str,
    pub(crate) groups: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) profiles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) member: Option<String>,
    pub(crate) summary: LsJsonSummary,
    pub(crate) items: Vec<LsJsonItem>,
    pub(crate) messages: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TreeJsonNode {
    pub(crate) key: String,
    pub(crate) id: String,
    pub(crate) source: String,
    pub(crate) version: String,
    pub(crate) groups: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TreeJsonEdge {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) constraint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TreeJsonReport {
    pub(crate) command: &'static str,
    pub(crate) groups: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) profiles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) member: Option<String>,
    pub(crate) mode: String,
    pub(crate) direction: String,
    pub(crate) roots: Vec<String>,
    pub(crate) nodes: Vec<TreeJsonNode>,
    pub(crate) edges: Vec<TreeJsonEdge>,
    pub(crate) messages: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WhyJsonStep {
    pub(crate) key: String,
    pub(crate) id: String,
    pub(crate) source: String,
    pub(crate) version: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WhyJsonTarget {
    pub(crate) key: String,
    pub(crate) id: String,
    pub(crate) source: String,
    pub(crate) version: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WhyJsonReport {
    pub(crate) command: &'static str,
    pub(crate) groups: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) profiles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) member: Option<String>,
    pub(crate) target: WhyJsonTarget,
    pub(crate) reason: String,
    pub(crate) direct: bool,
    pub(crate) paths: Vec<Vec<WhyJsonStep>>,
    pub(crate) messages: Vec<String>,
}

pub(crate) fn dependency_signature(dependency: &LockedDependency) -> String {
    let mut value = format!(
        "{} [{}] ({})",
        dependency.id,
        dependency.source.as_str(),
        dependency.kind.as_str()
    );
    if let Some(constraint) = dependency.constraint.as_deref() {
        value.push_str(&format!(" {constraint}"));
    }
    value
}

pub(crate) fn format_dependency_list(dependencies: &[LockedDependency]) -> String {
    if dependencies.is_empty() {
        return "-".to_string();
    }
    dependencies
        .iter()
        .map(dependency_signature)
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn dependency_to_json(dependency: &LockedDependency) -> LockDependencyJson {
    LockDependencyJson {
        source: dependency.source.as_str().to_string(),
        id: dependency.id.clone(),
        kind: dependency.kind.as_str().to_string(),
        constraint: dependency.constraint.clone(),
    }
}

pub(crate) fn lock_diff_entry_to_json(entry: &LockDiffEntry) -> LockDiffJsonEntry {
    LockDiffJsonEntry {
        kind: entry.kind.json_label().to_string(),
        id: entry.id.clone(),
        source: entry.source.as_str().to_string(),
        current_version: entry.current_version.clone(),
        desired_version: entry.desired_version.clone(),
        current_groups: entry.current_groups.clone(),
        desired_groups: entry.desired_groups.clone(),
        current_dependencies: entry
            .current_dependencies
            .iter()
            .map(dependency_to_json)
            .collect(),
        desired_dependencies: entry
            .desired_dependencies
            .iter()
            .map(dependency_to_json)
            .collect(),
        current_artifact: entry.current_artifact.clone(),
        desired_artifact: entry.desired_artifact.clone(),
    }
}

pub(crate) fn render_lock_diff_entry(entry: &LockDiffEntry) -> String {
    let prefix = format!(
        "{} {} [{}]",
        entry.kind.label(),
        entry.id,
        entry.source.as_str()
    );
    match entry.kind {
        LockDiffKind::Add => format!(
            "{prefix} -> {} groups={}",
            entry.desired_version.as_deref().unwrap_or("-"),
            format_group_list(&entry.desired_groups)
        ),
        LockDiffKind::Remove => format!(
            "{prefix} <- {} groups={}",
            entry.current_version.as_deref().unwrap_or("-"),
            format_group_list(&entry.current_groups)
        ),
        LockDiffKind::Upgrade | LockDiffKind::Downgrade | LockDiffKind::ChangeVersion => format!(
            "{prefix} {} -> {}",
            entry.current_version.as_deref().unwrap_or("-"),
            entry.desired_version.as_deref().unwrap_or("-")
        ),
        LockDiffKind::ChangeArtifact => format!(
            "{prefix} {} -> {}",
            entry.current_artifact.as_deref().unwrap_or("-"),
            entry.desired_artifact.as_deref().unwrap_or("-")
        ),
        LockDiffKind::ChangeGroups => format!(
            "{prefix} groups: {} -> {}",
            format_group_list(&entry.current_groups),
            format_group_list(&entry.desired_groups)
        ),
        LockDiffKind::ChangeDependencies => format!(
            "{prefix} dependencies: {} -> {}",
            format_dependency_list(&entry.current_dependencies),
            format_dependency_list(&entry.desired_dependencies)
        ),
    }
}

pub(crate) fn render_lock_write_report(report: LockWriteReport) -> CommandReport {
    CommandReport {
        output: format!(
            "lock updated: install={}, remove={}, unchanged={}\n",
            report.install, report.remove, report.unchanged
        ),
        exit_code: 0,
    }
}

pub(crate) fn emit_command_report(report: CommandReport) -> Result<()> {
    print!("{}", report.output);
    if !report.output.ends_with('\n') {
        println!();
    }
    std::io::stdout()
        .flush()
        .context("failed to flush stdout")?;
    if report.exit_code != 0 {
        process::exit(report.exit_code);
    }
    Ok(())
}

pub(crate) fn emit_json_report<T: Serialize>(report: &T, exit_code: i32) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(report).context("failed to encode json report")?
    );
    std::io::stdout()
        .flush()
        .context("failed to flush stdout")?;
    if exit_code != 0 {
        process::exit(exit_code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use mineconda_core::{DEFAULT_GROUP_NAME, ModSource};
    use serde_json::Value;

    use super::*;

    #[test]
    fn render_lock_diff_entry_formats_group_changes() {
        let line = render_lock_diff_entry(&LockDiffEntry {
            kind: LockDiffKind::ChangeGroups,
            id: "iris".to_string(),
            source: ModSource::Modrinth,
            current_version: Some("1.0.0".to_string()),
            desired_version: Some("1.0.0".to_string()),
            current_groups: vec![DEFAULT_GROUP_NAME.to_string()],
            desired_groups: vec![DEFAULT_GROUP_NAME.to_string(), "client".to_string()],
            current_dependencies: Vec::new(),
            desired_dependencies: Vec::new(),
            current_artifact: None,
            desired_artifact: None,
        });

        assert_eq!(
            line,
            "CHANGE GROUPS iris [modrinth] groups: default -> default,client"
        );
    }

    #[test]
    fn lock_diff_json_report_serializes_expected_shape() {
        let report = LockDiffJsonReport {
            command: "lock-diff",
            groups: vec![DEFAULT_GROUP_NAME.to_string(), "client".to_string()],
            summary: LockDiffJsonSummary {
                install: 1,
                remove: 0,
                unchanged: 2,
                changes: 1,
            },
            entries: vec![LockDiffJsonEntry {
                kind: "add".to_string(),
                id: "iris".to_string(),
                source: "modrinth".to_string(),
                current_version: None,
                desired_version: Some("1.0.0".to_string()),
                current_groups: Vec::new(),
                desired_groups: vec![DEFAULT_GROUP_NAME.to_string(), "client".to_string()],
                current_dependencies: Vec::new(),
                desired_dependencies: Vec::new(),
                current_artifact: None,
                desired_artifact: Some("file=iris.jar".to_string()),
            }],
        };

        let value: Value = serde_json::to_value(&report).expect("serialize report");
        assert_eq!(value["command"], "lock-diff");
        assert_eq!(value["groups"][0], "default");
        assert_eq!(value["summary"]["changes"], 1);
        assert_eq!(value["entries"][0]["kind"], "add");
        assert_eq!(value["entries"][0]["source"], "modrinth");
    }

    #[test]
    fn status_json_report_serializes_expected_shape() {
        let report = StatusJsonReport {
            command: "status",
            groups: vec![DEFAULT_GROUP_NAME.to_string()],
            summary: StatusJsonSummary {
                state: "clean",
                exit_code: 0,
            },
            manifest: StatusJsonManifest {
                exists: true,
                path: "/tmp/mineconda.toml".to_string(),
                roots: Some(1),
                named_groups: Some(0),
            },
            lockfile: StatusJsonLockfile {
                exists: true,
                path: "/tmp/mineconda.lock".to_string(),
                packages: Some(1),
                dependency_graph: Some(true),
                group_metadata: Some(true),
            },
            checks: StatusJsonChecks {
                project_metadata: "aligned",
                group_coverage: "ok",
                resolution: "up_to_date",
                sync: StatusJsonSync {
                    installed: Some(1),
                    missing: Some(0),
                    packages: Some(1),
                },
            },
            messages: vec!["status: groups=default".to_string()],
        };

        let value: Value = serde_json::to_value(&report).expect("serialize report");
        assert_eq!(value["command"], "status");
        assert_eq!(value["summary"]["state"], "clean");
        assert_eq!(value["manifest"]["exists"], true);
        assert_eq!(value["lockfile"]["dependency_graph"], true);
        assert_eq!(value["checks"]["resolution"], "up_to_date");
        assert_eq!(value["checks"]["sync"]["installed"], 1);
    }
}
