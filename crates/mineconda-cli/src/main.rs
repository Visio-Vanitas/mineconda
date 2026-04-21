use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Component, Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use mineconda_core::{
    DEFAULT_GROUP_NAME, JavaProvider, LoaderKind, LockedDependency, LockedDependencyKind,
    LockedPackage, Lockfile, Manifest, ModSide, ModSource, ModSpec, RuntimeProfile, S3CacheAuth,
    S3CacheConfig, S3SourceConfig, ServerProfile, WORKSPACE_FILE, WorkspaceConfig,
    is_default_group_name, is_valid_group_name, is_valid_profile_name, lockfile_path,
    manifest_path, read_lockfile, read_manifest, read_workspace, workspace_path, write_lockfile,
    write_manifest, write_workspace,
};
use mineconda_export::{
    ExportFormat, ExportRequest, ImportFormat as PackImportFormat, ImportRequest, ImportSide,
    OverrideScope, detect_pack_format, export_pack, import_pack_with_format,
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
use serde::Serialize;
use terminal_size::{Width, terminal_size};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

mod i18n;
mod search_tui;

#[derive(Parser, Debug)]
#[command(
    name = "mineconda",
    about = "Minecraft mod package manager inspired by uv"
)]
struct Cli {
    #[arg(long, global = true, default_value = ".")]
    root: PathBuf,
    #[arg(long, global = true)]
    workspace: bool,
    #[arg(long, global = true)]
    member: Option<String>,
    #[arg(long, global = true)]
    all_members: bool,
    #[arg(long = "profile", global = true)]
    profiles: Vec<String>,
    #[arg(long, global = true)]
    no_color: bool,
    #[arg(long, global = true, value_enum, default_value_t = LangArg::Auto)]
    lang: LangArg,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Init {
        name: String,
        #[arg(long, default_value = "1.20.1")]
        minecraft: String,
        #[arg(long, value_enum, default_value_t = LoaderArg::Fabric)]
        loader: LoaderArg,
        #[arg(long, default_value = "latest")]
        loader_version: String,
        #[arg(long)]
        bare: bool,
    },
    Add {
        id: String,
        #[arg(long, value_enum, default_value_t = SourceArg::Modrinth)]
        source: SourceArg,
        #[arg(long, default_value = "latest")]
        version: String,
        #[arg(long, value_enum, default_value_t = SideArg::Both)]
        side: SideArg,
        #[arg(long)]
        group: Option<String>,
        #[arg(long)]
        no_lock: bool,
    },
    Remove {
        id: String,
        #[arg(long, value_enum)]
        source: Option<SourceArg>,
        #[arg(long)]
        group: Option<String>,
        #[arg(long)]
        all_groups: bool,
        #[arg(long)]
        no_lock: bool,
    },
    Group {
        #[command(subcommand)]
        command: GroupCommands,
    },
    Profile {
        #[command(subcommand)]
        command: ProfileCommands,
    },
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommands,
    },
    Ls {
        #[arg(long)]
        status: bool,
        #[arg(long)]
        info: bool,
        #[arg(long = "group")]
        groups: Vec<String>,
        #[arg(long)]
        all_groups: bool,
        #[arg(long)]
        json: bool,
    },
    Search {
        query: String,
        #[arg(long, value_enum, default_value_t = SearchSourceArg::Modrinth)]
        source: SearchSourceArg,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long, default_value_t = 1)]
        page: usize,
        #[arg(long)]
        non_interactive: bool,
        #[arg(long)]
        install_first: bool,
        #[arg(long)]
        install_version: Option<String>,
        #[arg(long)]
        group: Option<String>,
    },
    Tree {
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        invert: Option<String>,
        #[arg(long, conflicts_with_all = ["id", "invert"])]
        all: bool,
        #[arg(long, value_enum)]
        source: Option<SourceArg>,
        #[arg(long = "group")]
        groups: Vec<String>,
        #[arg(long)]
        all_groups: bool,
        #[arg(long)]
        json: bool,
    },
    Why {
        id: String,
        #[arg(long, value_enum)]
        source: Option<SourceArg>,
        #[arg(long = "group")]
        groups: Vec<String>,
        #[arg(long)]
        all_groups: bool,
        #[arg(long)]
        json: bool,
    },
    #[command(visible_alias = "upgrade")]
    Update {
        id: Option<String>,
        #[arg(long, value_enum)]
        source: Option<SourceArg>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long = "group")]
        groups: Vec<String>,
        #[arg(long)]
        all_groups: bool,
        #[arg(long)]
        no_lock: bool,
    },
    Pin {
        id: String,
        #[arg(long, value_enum)]
        source: Option<SourceArg>,
        #[arg(long)]
        version: Option<String>,
        #[arg(long = "group")]
        groups: Vec<String>,
        #[arg(long)]
        all_groups: bool,
        #[arg(long)]
        no_lock: bool,
    },
    Lock {
        #[command(subcommand)]
        command: Option<LockCommands>,
        #[arg(long)]
        upgrade: bool,
        #[arg(long)]
        check: bool,
        #[arg(long = "group")]
        groups: Vec<String>,
        #[arg(long)]
        all_groups: bool,
    },
    Status {
        #[arg(long = "group")]
        groups: Vec<String>,
        #[arg(long)]
        all_groups: bool,
        #[arg(long)]
        json: bool,
    },
    Cache {
        #[command(subcommand)]
        command: CacheCommands,
    },
    Env {
        #[command(subcommand)]
        command: EnvCommands,
    },
    Sync {
        #[arg(long)]
        no_prune: bool,
        #[arg(long)]
        check: bool,
        #[arg(long)]
        locked: bool,
        #[arg(long)]
        frozen: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long, default_value_t = default_sync_jobs())]
        jobs: usize,
        #[arg(long)]
        verbose_cache: bool,
        #[arg(long = "group")]
        groups: Vec<String>,
        #[arg(long)]
        all_groups: bool,
    },
    Doctor {
        #[arg(long)]
        strict: bool,
    },
    Run {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        java: Option<String>,
        #[arg(long)]
        memory: Option<String>,
        #[arg(long = "jvm-arg")]
        jvm_args: Vec<String>,
        #[arg(long, value_enum, default_value_t = RunModeArg::Client)]
        mode: RunModeArg,
        #[arg(long, default_value = "DevPlayer")]
        username: String,
        #[arg(long, default_value = "dev")]
        instance: String,
        #[arg(long)]
        launcher_jar: Option<PathBuf>,
        #[arg(long)]
        server_jar: Option<PathBuf>,
        #[arg(long = "group")]
        groups: Vec<String>,
        #[arg(long)]
        all_groups: bool,
    },
    Export {
        #[arg(long, value_enum, default_value_t = ExportArg::Mrpack)]
        format: ExportArg,
        #[arg(long, default_value = "dist/modpack")]
        output: PathBuf,
        #[arg(long = "group")]
        groups: Vec<String>,
        #[arg(long)]
        all_groups: bool,
    },
    Import {
        input: String,
        #[arg(long, value_enum, default_value_t = ImportFormatArg::Auto)]
        format: ImportFormatArg,
        #[arg(long, value_enum, default_value_t = ImportSideArg::Client)]
        side: ImportSideArg,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
enum EnvCommands {
    Install {
        java: String,
        #[arg(long, value_enum, default_value_t = JavaProviderArg::Temurin)]
        provider: JavaProviderArg,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        use_for_project: bool,
    },
    Use {
        java: String,
        #[arg(long, value_enum, default_value_t = JavaProviderArg::Temurin)]
        provider: JavaProviderArg,
    },
    List,
    Which,
}

#[derive(Subcommand, Debug)]
enum GroupCommands {
    Ls,
    Add {
        name: String,
    },
    Remove {
        name: String,
        #[arg(long)]
        no_lock: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ProfileCommands {
    Ls,
    Add {
        name: String,
        #[arg(long = "group", required = true)]
        groups: Vec<String>,
    },
    Remove {
        name: String,
    },
}

#[derive(Subcommand, Debug)]
enum WorkspaceCommands {
    Init { name: String },
    Ls,
    Add { path: String },
    Remove { path: String },
}

#[derive(Subcommand, Debug)]
enum LockCommands {
    Diff {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum CacheCommands {
    Dir,
    Ls,
    Stats {
        #[arg(long)]
        json: bool,
    },
    Verify {
        #[arg(long)]
        repair: bool,
    },
    Clean,
    Purge,
    RemotePrune {
        #[arg(long)]
        s3: bool,
        #[arg(long)]
        max_age_days: u64,
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LoaderArg {
    Fabric,
    Forge,
    Neoforge,
    Quilt,
}

impl LoaderArg {
    fn to_core(self) -> mineconda_core::LoaderKind {
        match self {
            Self::Fabric => mineconda_core::LoaderKind::Fabric,
            Self::Forge => mineconda_core::LoaderKind::Forge,
            Self::Neoforge => mineconda_core::LoaderKind::NeoForge,
            Self::Quilt => mineconda_core::LoaderKind::Quilt,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SourceArg {
    Modrinth,
    Curseforge,
    Url,
    Local,
    S3,
}

impl SourceArg {
    fn to_core(self) -> ModSource {
        match self {
            Self::Modrinth => ModSource::Modrinth,
            Self::Curseforge => ModSource::Curseforge,
            Self::Url => ModSource::Url,
            Self::Local => ModSource::Local,
            Self::S3 => ModSource::S3,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SideArg {
    Both,
    Client,
    Server,
}

impl SideArg {
    fn to_core(self) -> ModSide {
        match self {
            Self::Both => ModSide::Both,
            Self::Client => ModSide::Client,
            Self::Server => ModSide::Server,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SearchSourceArg {
    Modrinth,
    Curseforge,
    Mcmod,
}

impl SearchSourceArg {
    fn to_core(self) -> SearchSource {
        match self {
            Self::Modrinth => SearchSource::Modrinth,
            Self::Curseforge => SearchSource::Curseforge,
            Self::Mcmod => SearchSource::Mcmod,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ExportArg {
    Curseforge,
    Mrpack,
    Multimc,
    #[value(name = "mods-desc")]
    ModsDesc,
}

impl ExportArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Curseforge => "curseforge",
            Self::Mrpack => "mrpack",
            Self::Multimc => "multimc",
            Self::ModsDesc => "mods-desc",
        }
    }

    fn to_core(self) -> ExportFormat {
        match self {
            Self::Curseforge => ExportFormat::CurseforgeZip,
            Self::Mrpack => ExportFormat::Mrpack,
            Self::Multimc => ExportFormat::MultiMcZip,
            Self::ModsDesc => ExportFormat::ModsDescriptionJson,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum JavaProviderArg {
    Temurin,
}

impl JavaProviderArg {
    fn to_core(self) -> JavaProvider {
        match self {
            Self::Temurin => JavaProvider::Temurin,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RunModeArg {
    Client,
    Server,
    Both,
}

impl RunModeArg {
    fn to_core(self) -> RunMode {
        match self {
            Self::Client => RunMode::Client,
            Self::Server => RunMode::Server,
            Self::Both => RunMode::Both,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ImportSideArg {
    Client,
    Server,
    Both,
}

impl ImportSideArg {
    fn to_core(self) -> ImportSide {
        match self {
            Self::Client => ImportSide::Client,
            Self::Server => ImportSide::Server,
            Self::Both => ImportSide::Both,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ImportFormatArg {
    Auto,
    Mrpack,
}

impl ImportFormatArg {
    fn to_core(self) -> Option<PackImportFormat> {
        match self {
            Self::Auto => None,
            Self::Mrpack => Some(PackImportFormat::Mrpack),
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LangArg {
    Auto,
    En,
    #[value(name = "zh-cn", alias = "zh")]
    ZhCn,
}

impl LangArg {
    fn to_preference(self) -> i18n::LangPreference {
        match self {
            Self::Auto => i18n::LangPreference::Auto,
            Self::En => i18n::LangPreference::En,
            Self::ZhCn => i18n::LangPreference::ZhCn,
        }
    }
}

fn default_sync_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|value| value.get().saturating_mul(2))
        .unwrap_or(4)
        .clamp(1, 8)
}

#[derive(Debug)]
struct RunCommandArgs {
    dry_run: bool,
    java: Option<String>,
    memory: Option<String>,
    jvm_args: Vec<String>,
    mode: RunModeArg,
    username: String,
    instance: String,
    launcher_jar: Option<PathBuf>,
    server_jar: Option<PathBuf>,
    groups: Vec<String>,
    all_groups: bool,
}

#[derive(Debug)]
struct SearchCommandArgs {
    query: String,
    source: SearchSourceArg,
    limit: usize,
    page: usize,
    no_color: bool,
    non_interactive: bool,
    install_first: bool,
    install_version: Option<String>,
    group: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ScopeArgs {
    workspace: bool,
    member: Option<String>,
    all_members: bool,
    profiles: Vec<String>,
}

#[derive(Debug, Clone)]
struct ProjectTarget {
    root: PathBuf,
    workspace: Option<WorkspaceConfig>,
    member_name: Option<String>,
}

#[derive(Debug, Clone)]
struct WorkspaceMemberTarget {
    name: String,
    root: PathBuf,
}

#[derive(Debug, Clone, Copy)]
struct ProjectSelection<'a> {
    groups: &'a [String],
    all_groups: bool,
    profiles: &'a [String],
    workspace: Option<&'a WorkspaceConfig>,
    member_name: Option<&'a str>,
}

impl<'a> ProjectSelection<'a> {
    fn active_groups(self, manifest: &Manifest) -> Result<BTreeSet<String>> {
        activation_groups_with_profiles(
            manifest,
            self.workspace,
            self.groups,
            self.all_groups,
            self.profiles,
        )
    }

    fn fallback_groups(self) -> Vec<String> {
        requested_groups_fallback(self.groups, self.all_groups)
    }

    fn normalized_profiles(self) -> Result<Vec<String>> {
        normalized_profile_names(self.profiles)
    }

    fn workspace_name(self) -> Option<String> {
        self.workspace.map(|item| item.workspace.name.clone())
    }
}

#[derive(Debug, Clone)]
struct TreeCommandArgs {
    id: Option<String>,
    invert: Option<String>,
    all: bool,
    source: Option<SourceArg>,
    json: bool,
}

#[derive(Debug, Clone)]
struct WhyCommandArgs {
    id: String,
    source: Option<SourceArg>,
    json: bool,
}

#[derive(Debug)]
struct SyncCommandArgs {
    prune: bool,
    check: bool,
    locked: bool,
    offline: bool,
    jobs: usize,
    verbose_cache: bool,
    groups: Vec<String>,
    all_groups: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum LockDiffKind {
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
struct LockDiffEntry {
    kind: LockDiffKind,
    id: String,
    source: ModSource,
    current_version: Option<String>,
    desired_version: Option<String>,
    current_groups: Vec<String>,
    desired_groups: Vec<String>,
    current_dependencies: Vec<LockedDependency>,
    desired_dependencies: Vec<LockedDependency>,
    current_artifact: Option<String>,
    desired_artifact: Option<String>,
}

#[derive(Debug)]
struct CommandReport {
    output: String,
    exit_code: i32,
}

#[derive(Debug, Clone, Serialize)]
struct JsonErrorReport {
    command: &'static str,
    groups: Vec<String>,
    error: String,
    exit_code: i32,
}

#[derive(Debug, Clone, Serialize)]
struct LockDiffJsonSummary {
    install: usize,
    remove: usize,
    unchanged: usize,
    changes: usize,
}

#[derive(Debug, Clone, Serialize)]
struct LockDependencyJson {
    source: String,
    id: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    constraint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct LockDiffJsonEntry {
    kind: String,
    id: String,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    desired_version: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    current_groups: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    desired_groups: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    current_dependencies: Vec<LockDependencyJson>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    desired_dependencies: Vec<LockDependencyJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_artifact: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    desired_artifact: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct LockDiffJsonReport {
    command: &'static str,
    groups: Vec<String>,
    summary: LockDiffJsonSummary,
    entries: Vec<LockDiffJsonEntry>,
}

#[derive(Debug, Clone, Serialize)]
struct StatusJsonSummary {
    state: &'static str,
    exit_code: i32,
}

#[derive(Debug, Clone, Serialize)]
struct StatusJsonManifest {
    exists: bool,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    roots: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    named_groups: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
struct StatusJsonLockfile {
    exists: bool,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    packages: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dependency_graph: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    group_metadata: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
struct StatusJsonSync {
    #[serde(skip_serializing_if = "Option::is_none")]
    installed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    missing: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    packages: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
struct StatusJsonChecks {
    project_metadata: &'static str,
    group_coverage: &'static str,
    resolution: &'static str,
    sync: StatusJsonSync,
}

#[derive(Debug, Clone, Serialize)]
struct StatusJsonReport {
    command: &'static str,
    groups: Vec<String>,
    summary: StatusJsonSummary,
    manifest: StatusJsonManifest,
    lockfile: StatusJsonLockfile,
    checks: StatusJsonChecks,
    messages: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct LsJsonSummary {
    roots: usize,
}

#[derive(Debug, Clone, Serialize)]
struct LsJsonItem {
    group: String,
    id: String,
    source: String,
    requested_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    locked_version: Option<String>,
    status: String,
    install_path: String,
    side: String,
}

#[derive(Debug, Clone, Serialize)]
struct LsJsonReport {
    command: &'static str,
    groups: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    profiles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    member: Option<String>,
    summary: LsJsonSummary,
    items: Vec<LsJsonItem>,
    messages: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct TreeJsonNode {
    key: String,
    id: String,
    source: String,
    version: String,
    groups: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct TreeJsonEdge {
    from: String,
    to: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    constraint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct TreeJsonReport {
    command: &'static str,
    groups: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    profiles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    member: Option<String>,
    mode: String,
    direction: String,
    roots: Vec<String>,
    nodes: Vec<TreeJsonNode>,
    edges: Vec<TreeJsonEdge>,
    messages: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct WhyJsonStep {
    key: String,
    id: String,
    source: String,
    version: String,
}

#[derive(Debug, Clone, Serialize)]
struct WhyJsonTarget {
    key: String,
    id: String,
    source: String,
    version: String,
}

#[derive(Debug, Clone, Serialize)]
struct WhyJsonReport {
    command: &'static str,
    groups: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    profiles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    member: Option<String>,
    target: WhyJsonTarget,
    reason: String,
    direct: bool,
    paths: Vec<Vec<WhyJsonStep>>,
    messages: Vec<String>,
}

fn normalize_group_selector(raw: &str) -> Result<String> {
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

fn normalize_named_group(raw: &str) -> Result<String> {
    let group = normalize_group_selector(raw)?;
    if is_default_group_name(&group) {
        bail!("`default` is the built-in root group and cannot be created or removed");
    }
    Ok(group)
}

fn normalize_profile_name(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("profile name must not be empty");
    }
    if !is_valid_profile_name(trimmed) {
        bail!("invalid profile name `{trimmed}` (expected lowercase kebab-case)");
    }
    Ok(trimmed.to_string())
}

fn normalize_member_entry(raw: &str) -> Result<String> {
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

fn profile_groups_for_selection(
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

fn activation_groups_with_profiles(
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

fn validate_manifest_profiles(manifest: &Manifest) -> Result<()> {
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

fn validate_workspace_config(workspace: &WorkspaceConfig) -> Result<()> {
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

fn load_workspace_optional(root: &Path) -> Result<Option<WorkspaceConfig>> {
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

fn load_workspace_required(root: &Path) -> Result<WorkspaceConfig> {
    load_workspace_optional(root)?.with_context(|| {
        format!(
            "workspace not found, expected {}",
            workspace_path(root).display()
        )
    })
}

fn workspace_member_target(
    workspace_root: &Path,
    workspace: &WorkspaceConfig,
    selector: &str,
) -> Result<WorkspaceMemberTarget> {
    let exact = normalize_member_entry(selector).ok();
    if let Some(exact) = exact {
        if workspace
            .member_entries()
            .iter()
            .any(|member| member == &exact)
        {
            return Ok(WorkspaceMemberTarget {
                name: exact.clone(),
                root: workspace_root.join(&exact),
            });
        }
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

fn workspace_members(
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

fn resolve_project_target(root: &Path, scope: &ScopeArgs) -> Result<ProjectTarget> {
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

fn target_group_name(group: Option<String>) -> Result<String> {
    match group {
        Some(group) => normalize_group_selector(&group),
        None => Ok(DEFAULT_GROUP_NAME.to_string()),
    }
}

fn activation_groups(
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

fn edit_groups(manifest: &Manifest, requested: &[String], all_groups: bool) -> Result<Vec<String>> {
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

fn package_in_groups(package: &LockedPackage, groups: &BTreeSet<String>) -> bool {
    if package.groups.is_empty() {
        return groups.contains(DEFAULT_GROUP_NAME);
    }
    package.groups.iter().any(|group| groups.contains(group))
}

fn selected_manifest_specs<'a>(
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

fn ensure_lock_group_metadata(
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

fn ensure_lock_covers_groups(
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

fn filtered_lockfile(lock: &Lockfile, groups: &BTreeSet<String>) -> Lockfile {
    let mut filtered = lock.clone();
    filtered
        .packages
        .retain(|package| package_in_groups(package, groups));
    filtered
}

fn filtered_manifest_for_export(manifest: &Manifest, groups: &BTreeSet<String>) -> Manifest {
    let mut filtered = manifest.clone();
    filtered.mods = selected_manifest_specs(manifest, groups)
        .into_iter()
        .map(|(_, spec)| spec.clone())
        .collect();
    filtered.groups = Default::default();
    filtered
}

fn format_group_list(groups: &[String]) -> String {
    if groups.is_empty() {
        return DEFAULT_GROUP_NAME.to_string();
    }
    groups.join(",")
}

fn format_active_groups(groups: &BTreeSet<String>) -> String {
    format_group_list(&groups.iter().cloned().collect::<Vec<_>>())
}

fn requested_groups_fallback(groups: &[String], all_groups: bool) -> Vec<String> {
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

fn format_selection_args(
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

fn format_selection_command(
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

fn normalized_profile_names(profiles: &[String]) -> Result<Vec<String>> {
    profiles
        .iter()
        .map(|profile| normalize_profile_name(profile))
        .collect()
}

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

fn dependency_signature(dependency: &LockedDependency) -> String {
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

fn format_dependency_list(dependencies: &[LockedDependency]) -> String {
    if dependencies.is_empty() {
        return "-".to_string();
    }
    dependencies
        .iter()
        .map(dependency_signature)
        .collect::<Vec<_>>()
        .join(", ")
}

fn dependency_to_json(dependency: &LockedDependency) -> LockDependencyJson {
    LockDependencyJson {
        source: dependency.source.as_str().to_string(),
        id: dependency.id.clone(),
        kind: dependency.kind.as_str().to_string(),
        constraint: dependency.constraint.clone(),
    }
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

fn lock_diff_entry_to_json(entry: &LockDiffEntry) -> LockDiffJsonEntry {
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

fn render_lock_diff_entry(entry: &LockDiffEntry) -> String {
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

fn workspace_aggregation_not_supported(command: &str) -> Result<()> {
    bail!(
        "workspace aggregation is currently supported only for `status` and `lock diff`; rerun `{command}` with `--member <path>`"
    )
}

fn main() -> Result<()> {
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
                    workspace_aggregation_not_supported(if check {
                        "mineconda lock --check"
                    } else {
                        "mineconda lock"
                    })?;
                }
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
                workspace_aggregation_not_supported(if check {
                    "mineconda sync --check"
                } else {
                    "mineconda sync"
                })?;
            }
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

fn cmd_init(
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

fn cmd_add(
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

fn cmd_remove(
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

fn cmd_group(root: &Path, command: GroupCommands) -> Result<()> {
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

fn cmd_profile(root: &Path, command: ProfileCommands, scope: &ScopeArgs) -> Result<()> {
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

fn cmd_workspace(root: &Path, command: WorkspaceCommands) -> Result<()> {
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

fn cmd_ls(
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

fn cmd_update(
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

fn cmd_pin(
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

fn lock_package_matches_spec(pkg: &LockedPackage, spec: &ModSpec) -> bool {
    lock_package_matches_request(pkg, spec.id.as_str(), Some(spec.source))
}

fn lock_package_matches_request(
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

fn cmd_search(root: &Path, args: SearchCommandArgs) -> Result<()> {
    let SearchCommandArgs {
        query,
        source,
        limit,
        page,
        no_color,
        non_interactive,
        install_first,
        install_version,
        group,
    } = args;
    let interactive = !non_interactive
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::env::var_os("CI").is_none();
    let manifest_env = if let Some(manifest) = load_manifest_optional(root)? {
        Some((manifest.project.minecraft, manifest.project.loader.kind))
    } else {
        None
    };
    let (minecraft_filter, loader_filter, environment_label) = if interactive {
        if let Some((minecraft, loader)) = manifest_env.as_ref() {
            (
                Some(minecraft.clone()),
                Some(*loader),
                Some(format!("{}/{}", minecraft, loader_label(*loader))),
            )
        } else {
            (None, None, None)
        }
    } else {
        (None, None, None)
    };

    let request = SearchRequest {
        source: source.to_core(),
        query: query.clone(),
        limit,
        page,
        minecraft_version: minecraft_filter.clone(),
        loader: loader_filter,
    };
    let spinner_enabled = std::io::stderr().is_terminal()
        && std::env::var_os("CI").is_none()
        && std::env::var_os("MINECONDA_NO_SPINNER").is_none();
    let spinner_label = if let Some(label) = environment_label.as_deref() {
        format!(
            "{} `{query}` @{} [{label}]",
            i18n::text("searching", "正在搜索"),
            request.source.as_str()
        )
    } else {
        format!(
            "{} `{query}` @{}",
            i18n::text("searching", "正在搜索"),
            request.source.as_str()
        )
    };
    let spinner = SearchSpinner::start(spinner_label, spinner_enabled);
    let results = match search_mods(&request) {
        Ok(results) => results,
        Err(err) if source == SearchSourceArg::Mcmod => {
            eprintln!(
                "{}: {}",
                i18n::text(
                    "warning: mcmod.cn request failed, fallback to modrinth",
                    "警告：mcmod.cn 请求失败，已降级到 modrinth"
                ),
                err
            );
            let fallback = SearchRequest {
                source: SearchSource::Modrinth,
                query: query.clone(),
                limit,
                page,
                minecraft_version: minecraft_filter.clone(),
                loader: loader_filter,
            };
            search_mods(&fallback)?
        }
        Err(err) => return Err(err),
    };
    drop(spinner);

    if results.is_empty() {
        println!("{}", i18n::text("no results", "无结果"));
        return Ok(());
    }

    if install_version.is_some() && !install_first && !interactive {
        bail!("--install-version requires --install-first or interactive install");
    }

    if install_first {
        install_search_selection(
            root,
            &results[0],
            install_version.as_deref(),
            manifest_env.as_ref().map(|(value, _)| value.as_str()),
            manifest_env.as_ref().map(|(_, value)| *value),
            group.as_deref(),
        )?;
        return Ok(());
    }

    if interactive {
        if let Some(install_request) = search_tui::run_search_interactive(
            &query,
            &results,
            no_color,
            environment_label.as_deref(),
        )? {
            let selected = &results[install_request.index];
            let version = if let Some(version) = install_version.clone() {
                Some(version)
            } else if install_request.choose_version {
                let (id, source) = resolve_search_install_target(selected)?;
                search_tui::choose_install_version_interactive(
                    &id,
                    source,
                    selected.title.as_str(),
                    no_color,
                    minecraft_filter.as_deref(),
                    loader_filter,
                )?
            } else {
                None
            };
            if install_request.choose_version && version.is_none() {
                println!("{}", i18n::text("installation cancelled", "已取消安装"));
                return Ok(());
            }
            install_search_selection(
                root,
                selected,
                version.as_deref(),
                manifest_env.as_ref().map(|(value, _)| value.as_str()),
                manifest_env.as_ref().map(|(_, value)| *value),
                group.as_deref(),
            )?;
        }
        return Ok(());
    }

    let use_color =
        std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() && !no_color;
    let layout = search_layout();
    println!(
        "{}",
        paint(
            &format!(
                "📋 {} ({})",
                i18n::text("Search Results", "搜索结果"),
                results.len()
            ),
            "1;35",
            use_color
        )
    );
    println!(
        "{}",
        paint(&"═".repeat(layout.total_width), "2;37", use_color)
    );
    for (index, item) in results.iter().enumerate() {
        print_search_result(item, index + 1, use_color, &layout);
    }

    Ok(())
}

fn loader_label(loader: LoaderKind) -> &'static str {
    match loader {
        LoaderKind::Fabric => "fabric",
        LoaderKind::Forge => "forge",
        LoaderKind::NeoForge => "neoforge",
        LoaderKind::Quilt => "quilt",
    }
}

fn to_run_loader_hint(loader: LoaderKind) -> LoaderHint {
    match loader {
        LoaderKind::Fabric => LoaderHint::Fabric,
        LoaderKind::Forge => LoaderHint::Forge,
        LoaderKind::NeoForge => LoaderHint::NeoForge,
        LoaderKind::Quilt => LoaderHint::Quilt,
    }
}

fn install_search_selection(
    root: &Path,
    item: &mineconda_resolver::SearchResult,
    version: Option<&str>,
    minecraft_version: Option<&str>,
    loader: Option<LoaderKind>,
    group: Option<&str>,
) -> Result<()> {
    let (id, source) = resolve_search_install_target(item)?;
    let mut manifest = load_manifest(root)?;
    let group = match group {
        Some(group) => normalize_group_selector(group)?,
        None => DEFAULT_GROUP_NAME.to_string(),
    };
    let side = item.supported_side.unwrap_or(ModSide::Both);
    let target_version =
        resolve_install_version_for_search(&id, source, version, minecraft_version, loader)?;

    if let Some(existing) = manifest
        .ensure_group_mods_mut(&group)
        .iter_mut()
        .find(|entry| entry.id == id && entry.source == source)
    {
        existing.version = target_version.to_string();
        existing.side = side;
    } else {
        manifest.ensure_group_mods_mut(&group).push(ModSpec::new(
            id.clone(),
            source,
            target_version.to_string(),
            side,
        ));
    }

    let path = manifest_path(root);
    write_manifest(&path, &manifest)
        .with_context(|| format!("failed to write {}", path.display()))?;
    let groups = if is_default_group_name(&group) {
        BTreeSet::new()
    } else {
        BTreeSet::from([group.clone()])
    };
    write_lock_from_manifest(root, &manifest, false, groups.clone())?;
    cmd_sync(
        root,
        SyncCommandArgs {
            prune: true,
            check: false,
            locked: false,
            offline: false,
            jobs: default_sync_jobs(),
            verbose_cache: false,
            groups: groups.into_iter().collect(),
            all_groups: false,
        },
        &[],
        None,
    )?;

    println!(
        "installed {} [{}]@{} and resolved dependencies",
        id,
        source.as_str(),
        target_version
    );
    Ok(())
}

fn resolve_install_version_for_search(
    id: &str,
    source: ModSource,
    explicit_version: Option<&str>,
    minecraft_version: Option<&str>,
    loader: Option<LoaderKind>,
) -> Result<String> {
    if let Some(version) = explicit_version {
        return Ok(version.to_string());
    }

    if !matches!(source, ModSource::Modrinth | ModSource::Curseforge) {
        return Ok("latest".to_string());
    }

    let versions = list_install_versions(&InstallVersionsRequest {
        source,
        id: id.to_string(),
        limit: 1,
        minecraft_version: minecraft_version.map(std::string::ToString::to_string),
        loader,
    });
    let versions = match versions {
        Ok(versions) => versions,
        Err(err) => {
            eprintln!(
                "warning: failed to preselect installable version for {} [{}], falling back to `latest`: {err}",
                id,
                source.as_str()
            );
            return Ok("latest".to_string());
        }
    };
    let selected = versions.into_iter().next().with_context(|| {
        format!(
            "no installable versions found for {} [{}]",
            id,
            source.as_str()
        )
    })?;
    Ok(selected.value)
}

fn resolve_search_install_target(
    item: &mineconda_resolver::SearchResult,
) -> Result<(String, ModSource)> {
    match item.source {
        SearchSource::Modrinth => {
            let id = optional_value(item.slug.as_str())
                .or_else(|| optional_value(item.id.as_str()))
                .context("selected Modrinth result has empty id/slug")?;
            Ok((id.to_string(), ModSource::Modrinth))
        }
        SearchSource::Curseforge => {
            if let Some(id) = optional_value(item.id.as_str())
                && id.chars().all(|ch| ch.is_ascii_digit())
            {
                return Ok((id.to_string(), ModSource::Curseforge));
            }
            if let Some(id) =
                optional_value(item.url.as_str()).and_then(extract_curseforge_project_id)
            {
                return Ok((id, ModSource::Curseforge));
            }
            bail!("selected CurseForge result is missing numeric project id")
        }
        SearchSource::Mcmod => {
            if let Some(url) = item.linked_modrinth_url.as_deref()
                && let Some(slug) = extract_modrinth_slug(url)
            {
                return Ok((slug, ModSource::Modrinth));
            }
            if let Some(url) = item.linked_curseforge_url.as_deref()
                && let Some(project_id) = extract_curseforge_project_id(url)
            {
                return Ok((project_id, ModSource::Curseforge));
            }
            bail!(
                "selected mcmod item has no installable Modrinth/CurseForge identifier; choose an item with source links"
            )
        }
    }
}

fn extract_modrinth_slug(url: &str) -> Option<String> {
    let normalized = url.split(['?', '#']).next().unwrap_or(url);
    let parts: Vec<&str> = normalized
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    for (index, part) in parts.iter().enumerate() {
        if matches!(
            *part,
            "mod" | "plugin" | "modpack" | "resourcepack" | "shader"
        ) && let Some(slug) = parts.get(index + 1)
            && !slug.is_empty()
        {
            return Some((*slug).to_string());
        }
    }
    None
}

fn extract_curseforge_project_id(url: &str) -> Option<String> {
    let normalized = url.split(['?', '#']).next().unwrap_or(url);
    for part in normalized.split('/') {
        if !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()) {
            return Some(part.to_string());
        }
    }

    if let Some(index) = normalized.find("modId=") {
        let value = &normalized[index + "modId=".len()..];
        let digits: String = value.chars().take_while(|ch| ch.is_ascii_digit()).collect();
        if !digits.is_empty() {
            return Some(digits);
        }
    }

    None
}

#[derive(Debug, Clone, Copy)]
struct SearchLayout {
    total_width: usize,
    two_col: bool,
    col_width: usize,
    content_width: usize,
}

struct SearchSpinner {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    enabled: bool,
}

impl SearchSpinner {
    fn start(label: String, enabled: bool) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        if !enabled {
            return Self {
                stop,
                handle: None,
                enabled: false,
            };
        }

        let stop_signal = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut index = 0usize;
            while !stop_signal.load(Ordering::Relaxed) {
                eprint!("\r{} {}", frames[index % frames.len()], label);
                let _ = std::io::stderr().flush();
                thread::sleep(Duration::from_millis(90));
                index += 1;
            }
        });

        Self {
            stop,
            handle: Some(handle),
            enabled: true,
        }
    }
}

impl Drop for SearchSpinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        if self.enabled {
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
        }
    }
}

fn search_layout() -> SearchLayout {
    let width = detect_terminal_width().clamp(60, 180);
    let two_col = width >= 96;
    let col_width = if two_col {
        width.saturating_sub(3) / 2
    } else {
        width.saturating_sub(2)
    };
    SearchLayout {
        total_width: width,
        two_col,
        col_width,
        content_width: width.saturating_sub(4).max(20),
    }
}

fn detect_terminal_width() -> usize {
    if let Some((Width(w), _)) = terminal_size()
        && w > 0
    {
        return w as usize;
    }

    if let Ok(raw) = std::env::var("COLUMNS")
        && let Ok(parsed) = raw.parse::<usize>()
        && parsed > 0
    {
        return parsed;
    }

    100
}

fn print_search_result(
    item: &mineconda_resolver::SearchResult,
    index: usize,
    use_color: bool,
    layout: &SearchLayout,
) {
    let downloads = item
        .downloads
        .map(|v| format!("📥 {}: {v}", i18n::text("Downloads", "下载")));
    let dependencies = if item.dependencies.is_empty() {
        None
    } else {
        Some(format!(
            "    🧩 {}: {}",
            i18n::text("Dependencies", "依赖"),
            item.dependencies.join(", ")
        ))
    };
    let homepage = optional_value(item.url.as_str())
        .map(|value| format!("🔗 {}: {value}", i18n::text("Homepage", "主页")));
    let supported_side = format_supported_side(item.supported_side);

    let title = paint(
        &format!(
            "🔎 {} #{index}  {}  ({})",
            i18n::text("Result", "结果"),
            item.title,
            item.source.as_str()
        ),
        "1;36",
        use_color,
    );
    let divider = paint(&"─".repeat(layout.total_width), "2;37", use_color);
    println!("{title}");
    println!("{divider}");

    print_two_col_optional(
        Some(format!(" └─ 🆔 ID: {}", item.id)),
        downloads,
        use_color,
        layout,
    );

    let slug = optional_value(item.slug.as_str()).map(|value| format!("    🏷️ Slug: {value}"));
    print_two_col_optional(
        slug,
        Some(format!(
            "🌐 {}: {}",
            i18n::text("Source", "源"),
            item.source.as_str()
        )),
        use_color,
        layout,
    );
    print_two_col_optional(dependencies, homepage, use_color, layout);

    if let Some(side) = supported_side {
        print_wrapped_kv(
            &format!("🖥️ {}:", i18n::text("Supported side", "适用端")),
            side,
            "0;37",
            use_color,
            layout,
        );
    }

    if let Some(summary) = optional_value(item.summary.as_str()) {
        println!(
            "{}",
            paint(
                &format!("📝 {}:", i18n::text("Summary", "简介")),
                "1;33",
                use_color
            )
        );
        for line in wrap_visual(summary, layout.content_width) {
            println!("  {}", paint(&line, "0;37", use_color));
        }
    }

    let link_items = [
        ("🌏 mcmod:", item.source_homepage.as_deref(), "36"),
        ("🌙 modrinth:", item.linked_modrinth_url.as_deref(), "32"),
        (
            "⚒️ curseforge:",
            item.linked_curseforge_url.as_deref(),
            "33",
        ),
        ("🐙 github:", item.linked_github_url.as_deref(), "1;34"),
    ];
    let has_any_link = link_items
        .iter()
        .any(|(_, value, _)| value.and_then(optional_value).is_some());
    if has_any_link {
        println!(
            "{}",
            paint(
                &format!("📚 {}:", i18n::text("Source links", "来源链接")),
                "1;35",
                use_color
            )
        );
        for (key, value, color) in link_items {
            if let Some(value) = value.and_then(optional_value) {
                print_wrapped_kv(key, value, color, use_color, layout);
            }
        }
    }
    println!();
}

fn print_two_col(left: &str, right: &str, use_color: bool, layout: &SearchLayout) {
    if layout.two_col {
        let left = truncate_visual(left, layout.col_width);
        let right = truncate_visual(right, layout.col_width);
        let left = pad_visual(&left, layout.col_width);
        let right = pad_visual(&right, layout.col_width);
        println!("{}", paint(&format!("{left} │ {right}"), "0;37", use_color));
        return;
    }

    for line in wrap_visual(left, layout.content_width) {
        println!("{}", paint(&line, "0;37", use_color));
    }
    for line in wrap_visual(right, layout.content_width) {
        println!("{}", paint(&line, "0;37", use_color));
    }
}

fn print_two_col_optional(
    left: Option<String>,
    right: Option<String>,
    use_color: bool,
    layout: &SearchLayout,
) {
    match (left, right) {
        (Some(left), Some(right)) => print_two_col(&left, &right, use_color, layout),
        (Some(line), None) | (None, Some(line)) => {
            for line in wrap_visual(line.trim(), layout.content_width) {
                println!("{}", paint(&line, "0;37", use_color));
            }
        }
        (None, None) => {}
    }
}

fn print_wrapped_kv(
    key: &str,
    value: &str,
    key_color: &str,
    use_color: bool,
    layout: &SearchLayout,
) {
    let Some(value) = optional_value(value) else {
        return;
    };
    let prefix = format!("  {key} ");
    let available = layout
        .total_width
        .saturating_sub(display_width(&prefix))
        .max(20);
    let wrapped = wrap_visual(value, available);
    if wrapped.is_empty() {
        println!("{} ", paint(&prefix, key_color, use_color));
        return;
    }

    println!(
        "{}{}",
        paint(&prefix, key_color, use_color),
        wrapped[0].as_str()
    );
    let indent = " ".repeat(display_width(&prefix));
    for line in wrapped.iter().skip(1) {
        println!("{indent}{line}");
    }
}

fn format_supported_side(side: Option<ModSide>) -> Option<&'static str> {
    side.map(|value| match value {
        ModSide::Both => i18n::text("both", "双端"),
        ModSide::Client => i18n::text("client", "客户端"),
        ModSide::Server => i18n::text("server", "服务端"),
    })
}

fn optional_value(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "-" {
        None
    } else {
        Some(trimmed)
    }
}

fn truncate_visual(input: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }

    if display_width(input) <= max_width {
        return input.to_string();
    }

    if max_width == 1 {
        return "…".to_string();
    }

    let mut value = String::new();
    let mut width = 0usize;
    let keep_width = max_width - 1;
    for ch in input.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if !value.is_empty() && width + ch_width > keep_width {
            break;
        }
        if value.is_empty() && ch_width > keep_width {
            break;
        }
        value.push(ch);
        width += ch_width;
    }
    value.push('…');
    value
}

fn wrap_visual(input: &str, max_width: usize) -> Vec<String> {
    if input.trim().is_empty() {
        return vec!["-".to_string()];
    }
    if max_width == 0 {
        return vec![input.to_string()];
    }

    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for word in input.split_whitespace() {
        let word_width = display_width(word);
        if word_width > max_width {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            lines.extend(split_by_width(word, max_width));
            continue;
        }

        let spacer = if current.is_empty() { 0 } else { 1 };
        if current_width + spacer + word_width > max_width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        if !current.is_empty() {
            current.push(' ');
            current_width += 1;
        }
        current.push_str(word);
        current_width += word_width;
    }

    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.extend(split_by_width(input, max_width));
    }
    if lines.is_empty() {
        lines.push("-".to_string());
    }
    lines
}

fn split_by_width(input: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![input.to_string()];
    }

    let mut out = Vec::new();
    let mut chunk = String::new();
    let mut chunk_width = 0usize;

    for ch in input.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if chunk_width + ch_width > max_width && !chunk.is_empty() {
            out.push(std::mem::take(&mut chunk));
            chunk_width = 0;
        }
        if ch_width > max_width {
            if !chunk.is_empty() {
                out.push(std::mem::take(&mut chunk));
                chunk_width = 0;
            }
            out.push(ch.to_string());
            continue;
        }
        chunk.push(ch);
        chunk_width += ch_width;
        if chunk_width >= max_width {
            out.push(std::mem::take(&mut chunk));
            chunk_width = 0;
        }
    }

    if !chunk.is_empty() {
        out.push(chunk);
    }
    if out.is_empty() {
        out.push(input.to_string());
    }
    out
}

fn display_width(input: &str) -> usize {
    UnicodeWidthStr::width(input)
}

fn pad_visual(input: &str, target_width: usize) -> String {
    let mut value = input.to_string();
    let pad = target_width.saturating_sub(display_width(input));
    if pad > 0 {
        value.push_str(&" ".repeat(pad));
    }
    value
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut idx = 0usize;
    while value >= 1024.0 && idx + 1 < UNITS.len() {
        value /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{} {}", bytes, UNITS[idx])
    } else {
        format!("{value:.1} {}", UNITS[idx])
    }
}

fn paint(text: &str, code: &str, enabled: bool) -> String {
    if !enabled {
        return text.to_string();
    }
    format!("\x1b[{code}m{text}\x1b[0m")
}

#[derive(Debug, Clone)]
struct LockGraphEdge {
    source: ModSource,
    id: String,
    kind: LockedDependencyKind,
    constraint: Option<String>,
}

#[derive(Debug, Clone)]
struct ReverseLockGraphEdge {
    dependent_key: String,
    kind: LockedDependencyKind,
    constraint: Option<String>,
}

struct LockGraph<'a> {
    packages_by_key: HashMap<String, &'a LockedPackage>,
    forward_edges: HashMap<String, Vec<LockGraphEdge>>,
    reverse_edges: HashMap<String, Vec<ReverseLockGraphEdge>>,
}

impl<'a> LockGraph<'a> {
    fn from_lock(lock: &'a Lockfile) -> Self {
        let mut packages_by_key = HashMap::new();
        let mut forward_edges = HashMap::new();
        let mut reverse_edges: HashMap<String, Vec<ReverseLockGraphEdge>> = HashMap::new();

        for package in &lock.packages {
            let key = lock_graph_key(package.source, &package.id);
            packages_by_key.insert(key.clone(), package);

            let mut edges: Vec<LockGraphEdge> = package
                .dependencies
                .iter()
                .map(|dependency| LockGraphEdge {
                    source: dependency.source,
                    id: dependency.id.clone(),
                    kind: dependency.kind,
                    constraint: dependency.constraint.clone(),
                })
                .collect();
            edges.sort_by(|left, right| {
                lock_graph_key(left.source, &left.id).cmp(&lock_graph_key(right.source, &right.id))
            });

            for edge in &edges {
                let target_key = lock_graph_key(edge.source, &edge.id);
                reverse_edges
                    .entry(target_key)
                    .or_default()
                    .push(ReverseLockGraphEdge {
                        dependent_key: key.clone(),
                        kind: edge.kind,
                        constraint: edge.constraint.clone(),
                    });
            }
            forward_edges.insert(key, edges);
        }

        for edges in reverse_edges.values_mut() {
            edges.sort_by(|left, right| left.dependent_key.cmp(&right.dependent_key));
        }

        Self {
            packages_by_key,
            forward_edges,
            reverse_edges,
        }
    }

    fn package(&self, key: &str) -> Option<&'a LockedPackage> {
        self.packages_by_key.get(key).copied()
    }
}

fn tree_json_node(package: &LockedPackage) -> TreeJsonNode {
    TreeJsonNode {
        key: locked_package_graph_key(package),
        id: package.id.clone(),
        source: package.source.as_str().to_string(),
        version: package.version.clone(),
        groups: normalized_package_groups(package),
    }
}

fn why_json_step(package: &LockedPackage) -> WhyJsonStep {
    WhyJsonStep {
        key: locked_package_graph_key(package),
        id: package.id.clone(),
        source: package.source.as_str().to_string(),
        version: package.version.clone(),
    }
}

fn collect_forward_tree_json(
    graph: &LockGraph<'_>,
    key: &str,
    path: &mut Vec<String>,
    nodes: &mut HashMap<String, TreeJsonNode>,
    edges: &mut HashMap<(String, String, String, Option<String>), TreeJsonEdge>,
) {
    if path.iter().any(|entry| entry == key) {
        return;
    }

    let Some(package) = graph.package(key) else {
        return;
    };
    nodes
        .entry(key.to_string())
        .or_insert_with(|| tree_json_node(package));

    path.push(key.to_string());
    for edge in graph.forward_edges.get(key).into_iter().flatten() {
        let child_key = lock_graph_key(edge.source, &edge.id);
        edges
            .entry((
                key.to_string(),
                child_key.clone(),
                edge.kind.as_str().to_string(),
                edge.constraint.clone(),
            ))
            .or_insert_with(|| TreeJsonEdge {
                from: key.to_string(),
                to: child_key.clone(),
                kind: edge.kind.as_str().to_string(),
                constraint: edge.constraint.clone(),
            });
        collect_forward_tree_json(graph, &child_key, path, nodes, edges);
    }
    path.pop();
}

fn collect_reverse_tree_json(
    graph: &LockGraph<'_>,
    key: &str,
    groups: &BTreeSet<String>,
    path: &mut Vec<String>,
    nodes: &mut HashMap<String, TreeJsonNode>,
    edges: &mut HashMap<(String, String, String, Option<String>), TreeJsonEdge>,
) {
    if path.iter().any(|entry| entry == key) {
        return;
    }

    let Some(package) = graph.package(key) else {
        return;
    };
    nodes
        .entry(key.to_string())
        .or_insert_with(|| tree_json_node(package));

    path.push(key.to_string());
    for edge in graph.reverse_edges.get(key).into_iter().flatten() {
        let Some(dependent) = graph.package(&edge.dependent_key) else {
            continue;
        };
        if !package_in_groups(dependent, groups) {
            continue;
        }
        edges
            .entry((
                key.to_string(),
                edge.dependent_key.clone(),
                edge.kind.as_str().to_string(),
                edge.constraint.clone(),
            ))
            .or_insert_with(|| TreeJsonEdge {
                from: key.to_string(),
                to: edge.dependent_key.clone(),
                kind: edge.kind.as_str().to_string(),
                constraint: edge.constraint.clone(),
            });
        collect_reverse_tree_json(graph, &edge.dependent_key, groups, path, nodes, edges);
    }
    path.pop();
}

fn build_tree_json_report(
    root: &Path,
    args: &TreeCommandArgs,
    selection: ProjectSelection<'_>,
) -> Result<TreeJsonReport> {
    let manifest = load_manifest(root)?;
    let lock = load_lockfile_required(root)?;
    ensure_lock_dependency_graph(&lock)?;
    let active_groups = selection.active_groups(&manifest)?;
    ensure_lock_covers_groups(&manifest, &lock, &active_groups)?;
    let graph = LockGraph::from_lock(&lock);
    let source_filter = args.source.map(SourceArg::to_core);
    let normalized_profiles = selection.normalized_profiles()?;

    let mut nodes = HashMap::new();
    let mut edges = HashMap::new();
    let mut path = Vec::new();
    let (mode, direction, roots): (String, String, Vec<String>) =
        if let Some(query) = args.invert.as_deref() {
            let target = resolve_locked_package(&lock, query, source_filter, &active_groups)?;
            let key = locked_package_graph_key(target);
            collect_reverse_tree_json(
                &graph,
                &key,
                &active_groups,
                &mut path,
                &mut nodes,
                &mut edges,
            );
            ("invert".to_string(), "reverse".to_string(), vec![key])
        } else if args.all {
            let packages = sorted_lock_packages(&lock, &active_groups);
            let keys = packages
                .iter()
                .map(|package| locked_package_graph_key(package))
                .collect::<Vec<_>>();
            for key in &keys {
                collect_forward_tree_json(&graph, key, &mut path, &mut nodes, &mut edges);
            }
            ("all".to_string(), "forward".to_string(), keys)
        } else if let Some(query) = args.id.as_deref() {
            let target = resolve_locked_package(&lock, query, source_filter, &active_groups)?;
            let key = locked_package_graph_key(target);
            collect_forward_tree_json(&graph, &key, &mut path, &mut nodes, &mut edges);
            ("target".to_string(), "forward".to_string(), vec![key])
        } else {
            let roots = resolve_manifest_root_packages(&manifest, &lock, &active_groups)?;
            let keys = roots
                .iter()
                .map(|package| locked_package_graph_key(package))
                .collect::<Vec<_>>();
            for key in &keys {
                collect_forward_tree_json(&graph, key, &mut path, &mut nodes, &mut edges);
            }
            ("roots".to_string(), "forward".to_string(), keys)
        };

    let mut node_values = nodes.into_values().collect::<Vec<_>>();
    node_values.sort_by(|left, right| left.key.cmp(&right.key));
    let mut edge_values = edges.into_values().collect::<Vec<_>>();
    edge_values.sort_by(|left, right| {
        (
            left.from.as_str(),
            left.to.as_str(),
            left.kind.as_str(),
            left.constraint.as_deref().unwrap_or(""),
        )
            .cmp(&(
                right.from.as_str(),
                right.to.as_str(),
                right.kind.as_str(),
                right.constraint.as_deref().unwrap_or(""),
            ))
    });

    Ok(TreeJsonReport {
        command: "tree",
        groups: active_groups.iter().cloned().collect(),
        profiles: normalized_profiles,
        workspace: selection.workspace_name(),
        member: selection.member_name.map(ToString::to_string),
        mode,
        direction,
        roots,
        nodes: node_values,
        edges: edge_values,
        messages: vec![format!(
            "selected groups={}",
            format_active_groups(&active_groups)
        )],
    })
}

fn cmd_tree(root: &Path, args: TreeCommandArgs, selection: ProjectSelection<'_>) -> Result<()> {
    let manifest = load_manifest(root)?;
    let lock = load_lockfile_required(root)?;
    ensure_lock_dependency_graph(&lock)?;
    let active_groups = selection.active_groups(&manifest)?;
    ensure_lock_covers_groups(&manifest, &lock, &active_groups)?;
    let graph = LockGraph::from_lock(&lock);
    let source_filter = args.source.map(SourceArg::to_core);

    if args.json {
        return emit_json_report(&build_tree_json_report(root, &args, selection)?, 0);
    }

    let output = if let Some(query) = args.invert.as_deref() {
        let target = resolve_locked_package(&lock, query, source_filter, &active_groups)?;
        render_reverse_dependency_tree(&graph, target, &active_groups)
    } else if args.all {
        let packages = sorted_lock_packages(&lock, &active_groups);
        render_forward_dependency_forest(&graph, &packages)
    } else if let Some(query) = args.id.as_deref() {
        let target = resolve_locked_package(&lock, query, source_filter, &active_groups)?;
        render_forward_dependency_forest(&graph, &[target])
    } else {
        let roots = resolve_manifest_root_packages(&manifest, &lock, &active_groups)?;
        render_forward_dependency_forest(&graph, &roots)
    };

    print!("{output}");
    Ok(())
}

fn cmd_why(root: &Path, args: WhyCommandArgs, selection: ProjectSelection<'_>) -> Result<()> {
    let manifest = load_manifest(root)?;
    let lock = load_lockfile_required(root)?;
    ensure_lock_dependency_graph(&lock)?;
    let active_groups = selection.active_groups(&manifest)?;
    ensure_lock_covers_groups(&manifest, &lock, &active_groups)?;
    let graph = LockGraph::from_lock(&lock);
    let roots = resolve_manifest_root_packages(&manifest, &lock, &active_groups)?;
    let target = resolve_locked_package(
        &lock,
        &args.id,
        args.source.map(SourceArg::to_core),
        &active_groups,
    )?;
    if args.json {
        return emit_json_report(&build_why_json_report(root, &args, selection)?, 0);
    }
    let output = render_why_report(&graph, &roots, target)?;
    print!("{output}");
    Ok(())
}

fn compute_why_paths(
    graph: &LockGraph<'_>,
    roots: &[&LockedPackage],
    target: &LockedPackage,
) -> Result<(bool, Vec<Vec<String>>)> {
    let target_key = locked_package_graph_key(target);
    let root_keys: Vec<String> = roots
        .iter()
        .map(|package| locked_package_graph_key(package))
        .collect();

    if root_keys.iter().any(|key| key == &target_key) {
        return Ok((true, vec![vec![target_key]]));
    }

    let mut distance: HashMap<String, usize> = HashMap::new();
    let mut predecessors: HashMap<String, Vec<String>> = HashMap::new();
    let mut queue = VecDeque::new();

    for root_key in &root_keys {
        distance.insert(root_key.clone(), 0);
        queue.push_back(root_key.clone());
    }

    while let Some(current) = queue.pop_front() {
        let current_distance = distance.get(&current).copied().unwrap_or(0);
        for edge in graph.forward_edges.get(&current).into_iter().flatten() {
            if edge.kind != LockedDependencyKind::Required {
                continue;
            }
            let next_key = lock_graph_key(edge.source, &edge.id);
            if graph.package(&next_key).is_none() {
                continue;
            }

            match distance.get(&next_key).copied() {
                None => {
                    distance.insert(next_key.clone(), current_distance + 1);
                    predecessors.insert(next_key.clone(), vec![current.clone()]);
                    queue.push_back(next_key);
                }
                Some(existing) if existing == current_distance + 1 => {
                    predecessors
                        .entry(next_key)
                        .or_default()
                        .push(current.clone());
                }
                _ => {}
            }
        }
    }

    if !distance.contains_key(&target_key) {
        bail!(
            "{} is locked but not reachable from manifest roots; rerun `mineconda lock`",
            locked_package_display(target)
        );
    }

    let mut paths = Vec::new();
    let mut current = vec![target_key.clone()];
    collect_why_paths(&predecessors, &root_keys, &mut current, &mut paths);
    paths.sort();
    paths.dedup();
    Ok((false, paths))
}

fn build_why_json_report(
    root: &Path,
    args: &WhyCommandArgs,
    selection: ProjectSelection<'_>,
) -> Result<WhyJsonReport> {
    let manifest = load_manifest(root)?;
    let lock = load_lockfile_required(root)?;
    ensure_lock_dependency_graph(&lock)?;
    let active_groups = selection.active_groups(&manifest)?;
    ensure_lock_covers_groups(&manifest, &lock, &active_groups)?;
    let graph = LockGraph::from_lock(&lock);
    let roots = resolve_manifest_root_packages(&manifest, &lock, &active_groups)?;
    let target = resolve_locked_package(
        &lock,
        &args.id,
        args.source.map(SourceArg::to_core),
        &active_groups,
    )?;
    let (direct, paths) = compute_why_paths(&graph, &roots, target)?;
    let target_step = why_json_step(target);
    Ok(WhyJsonReport {
        command: "why",
        groups: active_groups.iter().cloned().collect(),
        profiles: selection.normalized_profiles()?,
        workspace: selection.workspace_name(),
        member: selection.member_name.map(ToString::to_string),
        target: WhyJsonTarget {
            key: target_step.key.clone(),
            id: target_step.id.clone(),
            source: target_step.source.clone(),
            version: target_step.version.clone(),
        },
        reason: if direct {
            "direct".to_string()
        } else {
            "transitive".to_string()
        },
        direct,
        paths: paths
            .into_iter()
            .map(|path| {
                path.into_iter()
                    .filter_map(|key| graph.package(&key).map(why_json_step))
                    .collect::<Vec<_>>()
            })
            .collect(),
        messages: vec![format!(
            "selected groups={}",
            format_active_groups(&active_groups)
        )],
    })
}

fn render_why_report(
    graph: &LockGraph<'_>,
    roots: &[&LockedPackage],
    target: &LockedPackage,
) -> Result<String> {
    let (direct, paths) = compute_why_paths(graph, roots, target)?;
    if direct {
        return Ok(format!(
            "{} is a direct dependency\n{}\n",
            locked_package_display(target),
            locked_package_display(target)
        ));
    }
    let mut rendered_paths: Vec<String> = paths
        .into_iter()
        .map(|path| {
            path.into_iter()
                .map(|key| {
                    graph
                        .package(&key)
                        .map(locked_package_display)
                        .unwrap_or(key)
                })
                .collect::<Vec<_>>()
                .join(" -> ")
        })
        .collect();
    rendered_paths.sort();
    rendered_paths.dedup();

    let mut lines = vec![format!(
        "{} is a transitive dependency",
        locked_package_display(target)
    )];
    lines.extend(rendered_paths.into_iter().map(|path| format!("- {path}")));
    Ok(format!("{}\n", lines.join("\n")))
}

fn collect_why_paths(
    predecessors: &HashMap<String, Vec<String>>,
    root_keys: &[String],
    current_path: &mut Vec<String>,
    out: &mut Vec<Vec<String>>,
) {
    let Some(current_key) = current_path.last().cloned() else {
        return;
    };

    if root_keys.iter().any(|key| key == &current_key) {
        let mut path = current_path.clone();
        path.reverse();
        out.push(path);
        return;
    }

    let Some(previous) = predecessors.get(&current_key) else {
        return;
    };

    for predecessor in previous {
        current_path.push(predecessor.clone());
        collect_why_paths(predecessors, root_keys, current_path, out);
        current_path.pop();
    }
}

fn render_forward_dependency_forest(graph: &LockGraph<'_>, roots: &[&LockedPackage]) -> String {
    let mut lines = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        if index > 0 {
            lines.push(String::new());
        }
        let key = locked_package_graph_key(root);
        render_forward_tree_node(graph, &key, "", true, None, &mut Vec::new(), &mut lines);
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn render_reverse_dependency_tree(
    graph: &LockGraph<'_>,
    root: &LockedPackage,
    groups: &BTreeSet<String>,
) -> String {
    let mut state = ReverseTreeRenderState {
        groups,
        path: Vec::new(),
        lines: Vec::new(),
    };
    let key = locked_package_graph_key(root);
    render_reverse_tree_node(graph, &key, "", true, None, &mut state);
    if state.lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", state.lines.join("\n"))
    }
}

struct ReverseTreeRenderState<'a> {
    groups: &'a BTreeSet<String>,
    path: Vec<String>,
    lines: Vec<String>,
}

fn render_forward_tree_node(
    graph: &LockGraph<'_>,
    key: &str,
    prefix: &str,
    is_last: bool,
    incoming: Option<&LockGraphEdge>,
    path: &mut Vec<String>,
    lines: &mut Vec<String>,
) {
    let connector = if incoming.is_none() {
        ""
    } else if is_last {
        "`-- "
    } else {
        "|-- "
    };

    if path.iter().any(|entry| entry == key) {
        lines.push(format!("{prefix}{connector}{key} (cycle)"));
        return;
    }

    let label = if let Some(package) = graph.package(key) {
        locked_package_display(package)
    } else if let Some(edge) = incoming {
        format!("{} [{}] <not-locked>", edge.id, edge.source.as_str())
    } else {
        key.to_string()
    };
    let edge_label = incoming.map(format_tree_edge_label).unwrap_or_default();
    lines.push(format!("{prefix}{connector}{label}{edge_label}"));

    path.push(key.to_string());
    let child_prefix = if incoming.is_none() {
        String::new()
    } else if is_last {
        format!("{prefix}    ")
    } else {
        format!("{prefix}|   ")
    };

    let edges = graph.forward_edges.get(key).cloned().unwrap_or_default();
    for (index, edge) in edges.iter().enumerate() {
        let child_key = lock_graph_key(edge.source, &edge.id);
        render_forward_tree_node(
            graph,
            &child_key,
            &child_prefix,
            index + 1 == edges.len(),
            Some(edge),
            path,
            lines,
        );
    }
    path.pop();
}

fn render_reverse_tree_node(
    graph: &LockGraph<'_>,
    key: &str,
    prefix: &str,
    is_last: bool,
    incoming: Option<&ReverseLockGraphEdge>,
    state: &mut ReverseTreeRenderState<'_>,
) {
    let connector = if incoming.is_none() {
        ""
    } else if is_last {
        "`-- "
    } else {
        "|-- "
    };

    if state.path.iter().any(|entry| entry == key) {
        state
            .lines
            .push(format!("{prefix}{connector}{key} (cycle)"));
        return;
    }

    let label = graph
        .package(key)
        .map(locked_package_display)
        .unwrap_or_else(|| key.to_string());
    let edge_label = incoming
        .map(format_reverse_tree_edge_label)
        .unwrap_or_default();
    state
        .lines
        .push(format!("{prefix}{connector}{label}{edge_label}"));

    state.path.push(key.to_string());
    let child_prefix = if incoming.is_none() {
        String::new()
    } else if is_last {
        format!("{prefix}    ")
    } else {
        format!("{prefix}|   ")
    };

    let edges = graph.reverse_edges.get(key).cloned().unwrap_or_default();
    let visible_edges: Vec<ReverseLockGraphEdge> = edges
        .into_iter()
        .filter(|edge| {
            graph
                .package(&edge.dependent_key)
                .is_some_and(|package| package_in_groups(package, state.groups))
        })
        .collect();
    for (index, edge) in visible_edges.iter().enumerate() {
        render_reverse_tree_node(
            graph,
            &edge.dependent_key,
            &child_prefix,
            index + 1 == visible_edges.len(),
            Some(edge),
            state,
        );
    }
    state.path.pop();
}

fn format_tree_edge_label(edge: &LockGraphEdge) -> String {
    let mut suffix = edge.kind.as_str().to_string();
    if let Some(constraint) = edge.constraint.as_deref() {
        suffix.push_str(&format!(", {constraint}"));
    }
    format!(" ({suffix})")
}

fn format_reverse_tree_edge_label(edge: &ReverseLockGraphEdge) -> String {
    let mut suffix = format!("depended on via {}", edge.kind.as_str());
    if let Some(constraint) = edge.constraint.as_deref() {
        suffix.push_str(&format!(", {constraint}"));
    }
    format!(" ({suffix})")
}

fn ensure_lock_dependency_graph(lock: &Lockfile) -> Result<()> {
    if lock.metadata.dependency_graph {
        return Ok(());
    }
    bail!("lockfile does not contain dependency graph data; rerun `mineconda lock` first")
}

fn resolve_manifest_root_packages<'a>(
    manifest: &Manifest,
    lock: &'a Lockfile,
    groups: &BTreeSet<String>,
) -> Result<Vec<&'a LockedPackage>> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();

    for (group, spec) in selected_manifest_specs(manifest, groups) {
        let single_group = BTreeSet::from([group.clone()]);
        let package = lock
            .packages
            .iter()
            .find(|package| package_in_groups(package, &single_group) && lock_package_matches_spec(package, spec))
            .with_context(|| {
                format!(
                    "manifest entry `{}` [{}] in group `{}` is not present in lockfile; rerun `mineconda lock`",
                    spec.id, spec.source.as_str(), group
                )
            })?;
        let key = locked_package_graph_key(package);
        if seen.insert(key) {
            roots.push(package);
        }
    }

    Ok(roots)
}

fn resolve_locked_package<'a>(
    lock: &'a Lockfile,
    id: &str,
    source: Option<ModSource>,
    groups: &BTreeSet<String>,
) -> Result<&'a LockedPackage> {
    let matches: Vec<&LockedPackage> = lock
        .packages
        .iter()
        .filter(|package| {
            package_in_groups(package, groups) && lock_package_matches_request(package, id, source)
        })
        .collect();

    match matches.as_slice() {
        [] => bail!("package `{id}` not found in lockfile"),
        [package] => Ok(*package),
        _ => bail!("multiple lockfile entries match `{id}`, use `--source` to disambiguate"),
    }
}

fn sorted_lock_packages<'a>(
    lock: &'a Lockfile,
    groups: &BTreeSet<String>,
) -> Vec<&'a LockedPackage> {
    let mut packages: Vec<&LockedPackage> = lock
        .packages
        .iter()
        .filter(|package| package_in_groups(package, groups))
        .collect();
    packages.sort_by(|left, right| {
        locked_package_graph_key(left).cmp(&locked_package_graph_key(right))
    });
    packages
}

fn lock_graph_key(source: ModSource, id: &str) -> String {
    format!("{}@{}", id, source.as_str())
}

fn locked_package_graph_key(package: &LockedPackage) -> String {
    lock_graph_key(package.source, &package.id)
}

fn locked_package_display(package: &LockedPackage) -> String {
    format!(
        "{} [{}] {}",
        package.id,
        package.source.as_str(),
        package.version
    )
}

fn cmd_lock(
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

fn build_lock_check_report(
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

fn emit_command_report(report: CommandReport) -> Result<()> {
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

fn emit_json_report<T: Serialize>(report: &T, exit_code: i32) -> Result<()> {
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

fn command_display_name(command: &str) -> &str {
    match command {
        "lock-diff" => "lock diff",
        "status" => "status",
        _ => command,
    }
}

fn json_error_report(
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

fn render_json_error_report(report: &JsonErrorReport) -> CommandReport {
    CommandReport {
        output: format!("{}\n", report.error),
        exit_code: report.exit_code,
    }
}

fn lock_diff_report_body_lines(report: &LockDiffJsonReport) -> Vec<String> {
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

fn render_lock_diff_json_report(report: &LockDiffJsonReport) -> CommandReport {
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

fn render_status_json_report(report: &StatusJsonReport) -> CommandReport {
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

fn build_lock_diff_json_report(
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

fn cmd_lock_diff(
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

fn build_status_json_report(
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

fn cmd_status(root: &Path, json: bool, selection: ProjectSelection<'_>) -> Result<()> {
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

#[derive(Debug, Clone, Serialize)]
struct WorkspaceAggregateSummary {
    members: usize,
    changed: usize,
    failed: usize,
    exit_code: i32,
}

#[derive(Debug, Clone, Serialize)]
struct WorkspaceAggregateMemberJson {
    member: String,
    path: String,
    exit_code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct WorkspaceAggregateJsonReport {
    command: &'static str,
    workspace: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    groups: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    profiles: Vec<String>,
    summary: WorkspaceAggregateSummary,
    members: Vec<WorkspaceAggregateMemberJson>,
}

fn cmd_status_workspace(
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

fn cmd_lock_diff_workspace(
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

fn cmd_cache(root: &Path, command: CacheCommands) -> Result<()> {
    match command {
        CacheCommands::Dir => {
            let cache = cache_root_path()?;
            println!("{}", cache.display());
            Ok(())
        }
        CacheCommands::Ls => cmd_cache_ls(root),
        CacheCommands::Stats { json } => cmd_cache_stats(root, json),
        CacheCommands::Verify { repair } => cmd_cache_verify(root, repair),
        CacheCommands::Clean => cmd_cache_clean(root),
        CacheCommands::Purge => cmd_cache_purge(),
        CacheCommands::RemotePrune {
            s3,
            max_age_days,
            prefix,
            dry_run,
        } => cmd_cache_remote_prune(root, s3, max_age_days, prefix, dry_run),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DoctorLevel {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Default)]
struct DoctorCounts {
    ok: usize,
    warn: usize,
    fail: usize,
}

impl DoctorCounts {
    fn push(&mut self, level: DoctorLevel) {
        match level {
            DoctorLevel::Ok => self.ok += 1,
            DoctorLevel::Warn => self.warn += 1,
            DoctorLevel::Fail => self.fail += 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DoctorFinding {
    level: DoctorLevel,
    title: &'static str,
    detail: String,
}

impl DoctorFinding {
    fn new(level: DoctorLevel, title: &'static str, detail: impl Into<String>) -> Self {
        Self {
            level,
            title,
            detail: detail.into(),
        }
    }
}

fn collect_s3_doctor_findings<F>(manifest: &Manifest, mut has_env: F) -> Vec<DoctorFinding>
where
    F: FnMut(&str) -> bool,
{
    let has_s3_source_mods = manifest
        .mods
        .iter()
        .any(|entry| entry.source == ModSource::S3);
    let Some(s3_cache) = manifest.cache.s3.as_ref() else {
        if !has_s3_source_mods {
            return Vec::new();
        }
        let mut findings = Vec::new();
        findings.push(DoctorFinding::new(
            DoctorLevel::Ok,
            "s3 status",
            "experimental feature; not part of the stable baseline",
        ));
        findings.extend(collect_s3_source_doctor_findings(
            manifest.sources.s3.as_ref(),
            has_s3_source_mods,
        ));
        return findings;
    };

    let mut findings = vec![DoctorFinding::new(
        DoctorLevel::Ok,
        "s3 status",
        "experimental feature; not part of the stable baseline",
    )];
    findings.extend(collect_s3_source_doctor_findings(
        manifest.sources.s3.as_ref(),
        has_s3_source_mods,
    ));
    findings.extend(collect_s3_cache_doctor_findings(s3_cache, &mut has_env));
    findings
}

fn collect_s3_source_doctor_findings(
    source: Option<&S3SourceConfig>,
    has_s3_source_mods: bool,
) -> Vec<DoctorFinding> {
    if !has_s3_source_mods {
        return Vec::new();
    }

    let finding = match source {
        Some(s3) if !s3.bucket.trim().is_empty() => {
            let mut detail = format!("bucket={}", s3.bucket);
            if let Some(prefix) = s3
                .key_prefix
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                detail.push_str(&format!(" prefix={prefix}"));
            }
            DoctorFinding::new(DoctorLevel::Ok, "s3 source config", detail)
        }
        _ => DoctorFinding::new(
            DoctorLevel::Warn,
            "s3 source config",
            "mods use source=s3 but [sources.s3] is missing/invalid",
        ),
    };

    vec![finding]
}

fn collect_s3_cache_doctor_findings<F>(
    s3_cache: &S3CacheConfig,
    mut has_env: F,
) -> Vec<DoctorFinding>
where
    F: FnMut(&str) -> bool,
{
    let mut findings = Vec::new();

    if !s3_cache.enabled {
        findings.push(DoctorFinding::new(
            DoctorLevel::Ok,
            "s3 cache config",
            "configured but disabled",
        ));
    } else if s3_cache.bucket.trim().is_empty() {
        findings.push(DoctorFinding::new(
            DoctorLevel::Warn,
            "s3 cache config",
            "cache.s3.enabled=true but bucket is empty",
        ));
    } else {
        findings.push(DoctorFinding::new(
            DoctorLevel::Ok,
            "s3 cache config",
            format!("enabled bucket={}", s3_cache.bucket),
        ));
    }

    findings.push(DoctorFinding::new(
        DoctorLevel::Ok,
        "s3 cache auth",
        format!("mode={}", s3_cache.auth.as_str()),
    ));

    if s3_cache
        .public_base_url
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty())
        && s3_cache
            .endpoint
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
    {
        findings.push(DoctorFinding::new(
            DoctorLevel::Warn,
            "s3 cache endpoint",
            "public_base_url and endpoint are both set; public_base_url will be used first",
        ));
    }

    if matches!(s3_cache.auth, S3CacheAuth::Sigv4)
        && s3_cache
            .public_base_url
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
    {
        findings.push(DoctorFinding::new(
            DoctorLevel::Warn,
            "s3 cache auth",
            "sigv4 ignores public_base_url for signed requests; prefer endpoint/path_style",
        ));
    }

    for (field, name) in [
        ("access_key_env", s3_cache.access_key_env.as_deref()),
        ("secret_key_env", s3_cache.secret_key_env.as_deref()),
        ("session_token_env", s3_cache.session_token_env.as_deref()),
    ] {
        let Some(name) = name else {
            continue;
        };

        if name.trim().is_empty() {
            findings.push(DoctorFinding::new(
                DoctorLevel::Warn,
                "s3 cache credential",
                format!("cache.s3.{field} is empty"),
            ));
        } else if has_env(name) {
            findings.push(DoctorFinding::new(
                DoctorLevel::Ok,
                "s3 cache credential",
                format!("{name} is set"),
            ));
        } else {
            findings.push(DoctorFinding::new(
                DoctorLevel::Warn,
                "s3 cache credential",
                format!("{name} is not set"),
            ));
        }
    }

    let has_access = s3_cache
        .access_key_env
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    let has_secret = s3_cache
        .secret_key_env
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    if has_access ^ has_secret {
        findings.push(DoctorFinding::new(
            DoctorLevel::Warn,
            "s3 cache credential",
            "access_key_env and secret_key_env should be configured together",
        ));
    }

    if matches!(s3_cache.auth, S3CacheAuth::Sigv4) && !(has_access && has_secret) {
        findings.push(DoctorFinding::new(
            DoctorLevel::Warn,
            "s3 cache auth",
            "cache.s3.auth=sigv4 expects access_key_env and secret_key_env",
        ));
    }

    if matches!(s3_cache.auth, S3CacheAuth::Sigv4)
        && s3_cache
            .region
            .as_deref()
            .map(str::trim)
            .is_none_or(|value| value.is_empty())
    {
        findings.push(DoctorFinding::new(
            DoctorLevel::Warn,
            "s3 cache auth",
            "cache.s3.auth=sigv4 will default region to us-east-1",
        ));
    }

    findings
}

fn cmd_doctor(root: &Path, strict: bool, no_color: bool) -> Result<()> {
    let use_color =
        std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() && !no_color;
    let mut counts = DoctorCounts::default();

    let manifest_path = manifest_path(root);
    let manifest = if !manifest_path.exists() {
        doctor_log(
            &mut counts,
            DoctorLevel::Fail,
            "manifest missing",
            format!("{} not found", manifest_path.display()),
            use_color,
        );
        None
    } else {
        match load_manifest(root) {
            Ok(manifest) => {
                doctor_log(
                    &mut counts,
                    DoctorLevel::Ok,
                    "manifest",
                    format!("{} (mods={})", manifest_path.display(), manifest.mods.len()),
                    use_color,
                );
                Some(manifest)
            }
            Err(err) => {
                doctor_log(
                    &mut counts,
                    DoctorLevel::Fail,
                    "manifest parse",
                    format!("{err:#}"),
                    use_color,
                );
                None
            }
        }
    };

    let lock_path = lockfile_path(root);
    let lock = if !lock_path.exists() {
        doctor_log(
            &mut counts,
            DoctorLevel::Warn,
            "lockfile",
            format!("{} not found (run `mineconda lock`)", lock_path.display()),
            use_color,
        );
        None
    } else {
        match load_lockfile_required(root) {
            Ok(lock) => {
                doctor_log(
                    &mut counts,
                    DoctorLevel::Ok,
                    "lockfile",
                    format!("{} (packages={})", lock_path.display(), lock.packages.len()),
                    use_color,
                );
                Some(lock)
            }
            Err(err) => {
                doctor_log(
                    &mut counts,
                    DoctorLevel::Fail,
                    "lockfile parse",
                    format!("{err:#}"),
                    use_color,
                );
                None
            }
        }
    };

    if let (Some(manifest), Some(lock)) = (manifest.as_ref(), lock.as_ref()) {
        if lock.metadata.minecraft != manifest.project.minecraft
            || lock.metadata.loader.kind != manifest.project.loader.kind
            || lock.metadata.loader.version != manifest.project.loader.version
        {
            doctor_log(
                &mut counts,
                DoctorLevel::Fail,
                "manifest/lock consistency",
                "lock metadata does not match project minecraft/loader".to_string(),
                use_color,
            );
        } else {
            doctor_log(
                &mut counts,
                DoctorLevel::Ok,
                "manifest/lock consistency",
                "metadata aligned".to_string(),
                use_color,
            );
        }
    }

    if let Some(manifest) = manifest.as_ref() {
        let has_curseforge = manifest
            .mods
            .iter()
            .any(|entry| entry.source == ModSource::Curseforge);
        if has_curseforge {
            if env::var_os("CURSEFORGE_API_KEY").is_some() {
                doctor_log(
                    &mut counts,
                    DoctorLevel::Ok,
                    "curseforge credential",
                    "CURSEFORGE_API_KEY is set".to_string(),
                    use_color,
                );
            } else {
                doctor_log(
                    &mut counts,
                    DoctorLevel::Warn,
                    "curseforge credential",
                    "CURSEFORGE_API_KEY is not set".to_string(),
                    use_color,
                );
            }
        }

        for finding in collect_s3_doctor_findings(manifest, |name| env::var_os(name).is_some()) {
            doctor_log(
                &mut counts,
                finding.level,
                finding.title,
                finding.detail,
                use_color,
            );
        }

        if manifest.server.java != "java" {
            if java_command_exists(manifest.server.java.as_str(), root) {
                doctor_log(
                    &mut counts,
                    DoctorLevel::Ok,
                    "server java command",
                    manifest.server.java.clone(),
                    use_color,
                );
            } else {
                doctor_log(
                    &mut counts,
                    DoctorLevel::Warn,
                    "server java command",
                    format!("`{}` not found on filesystem/PATH", manifest.server.java),
                    use_color,
                );
            }
        }

        match find_java_runtime(&manifest.runtime.java, manifest.runtime.provider)? {
            Some(path) => {
                doctor_log(
                    &mut counts,
                    DoctorLevel::Ok,
                    "managed runtime",
                    format!(
                        "java {} ({}) -> {}",
                        manifest.runtime.java,
                        manifest.runtime.provider.as_str(),
                        path.display()
                    ),
                    use_color,
                );
            }
            None if manifest.runtime.auto_install => {
                doctor_log(
                    &mut counts,
                    DoctorLevel::Warn,
                    "managed runtime",
                    format!(
                        "java {} ({}) is not installed, will auto-install on demand",
                        manifest.runtime.java,
                        manifest.runtime.provider.as_str()
                    ),
                    use_color,
                );
            }
            None => {
                doctor_log(
                    &mut counts,
                    DoctorLevel::Fail,
                    "managed runtime",
                    format!(
                        "java {} ({}) is not installed and auto_install=false",
                        manifest.runtime.java,
                        manifest.runtime.provider.as_str()
                    ),
                    use_color,
                );
            }
        }
    }

    match cache_root_path() {
        Ok(path) => match fs::create_dir_all(&path) {
            Ok(_) => doctor_log(
                &mut counts,
                DoctorLevel::Ok,
                "cache dir",
                path.display().to_string(),
                use_color,
            ),
            Err(err) => doctor_log(
                &mut counts,
                DoctorLevel::Fail,
                "cache dir",
                format!("{} ({err})", path.display()),
                use_color,
            ),
        },
        Err(err) => doctor_log(
            &mut counts,
            DoctorLevel::Fail,
            "cache dir",
            format!("{err:#}"),
            use_color,
        ),
    }

    println!(
        "{}",
        paint(
            &format!(
                "doctor summary: ok={}, warn={}, fail={}",
                counts.ok, counts.warn, counts.fail
            ),
            if counts.fail > 0 {
                "1;31"
            } else if counts.warn > 0 {
                "1;33"
            } else {
                "1;32"
            },
            use_color
        )
    );

    if counts.fail > 0 {
        bail!("doctor detected {} blocking issues", counts.fail);
    }
    if strict && counts.warn > 0 {
        bail!("doctor strict mode failed on {} warnings", counts.warn);
    }
    Ok(())
}

fn doctor_log(
    counts: &mut DoctorCounts,
    level: DoctorLevel,
    title: &str,
    detail: String,
    use_color: bool,
) {
    counts.push(level);
    let (tag, color) = match level {
        DoctorLevel::Ok => ("ok", "1;32"),
        DoctorLevel::Warn => ("warn", "1;33"),
        DoctorLevel::Fail => ("fail", "1;31"),
    };
    println!(
        "[{}] {}: {}",
        paint(tag, color, use_color),
        title,
        truncate_visual(&detail, 200)
    );
}

fn java_command_exists(command: &str, root: &Path) -> bool {
    let raw = command.trim();
    if raw.is_empty() {
        return false;
    }

    if raw.contains('/') {
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            return path.exists();
        }
        return root.join(&path).exists() || path.exists();
    }

    if let Some(path_env) = env::var_os("PATH") {
        for dir in env::split_paths(&path_env) {
            let candidate = dir.join(raw);
            if candidate.exists() {
                return true;
            }
        }
    }
    false
}

fn cmd_sync(
    root: &Path,
    args: SyncCommandArgs,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<()> {
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
        return emit_command_report(build_sync_check_report(
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
        )?);
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
        println!("lockfile metadata updated: {}", path.display());
    }

    println!(
        "sync done: packages={}, local_hits={}, s3_hits={}, origin_downloads={}, installed={}, removed={}, failed={}",
        report.package_count,
        report.local_hits,
        report.s3_hits,
        report.origin_downloads,
        report.installed,
        report.removed,
        report.failed
    );
    Ok(())
}

fn build_sync_check_report(
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

    if missing.is_empty() {
        return Ok(CommandReport {
            output: format!(
                "sync check: installed groups={group_label} installed={installed} missing=0 packages={package_count}\n"
            ),
            exit_code: 0,
        });
    }

    let mut lines = vec![format!(
        "sync check: missing groups={group_label} installed={installed} missing={} packages={package_count}",
        missing.len()
    )];
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

fn cmd_cache_ls(root: &Path) -> Result<()> {
    let cache_root = cache_root_path()?;
    if !cache_root.exists() {
        println!("cache directory does not exist: {}", cache_root.display());
        return Ok(());
    }

    let lock = load_lockfile_optional(root)?;
    let used_paths = expected_cache_paths(lock.as_ref(), &cache_root);
    let mut files = Vec::new();
    for entry in fs::read_dir(&cache_root)
        .with_context(|| format!("failed to read {}", cache_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().to_string();
        let size = entry.metadata()?.len();
        let used = used_paths.contains(&path);
        files.push((file_name, size, used));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    if files.is_empty() {
        println!("cache is empty: {}", cache_root.display());
        return Ok(());
    }

    let mut total_bytes = 0u64;
    for (file_name, size, used) in files {
        total_bytes += size;
        let marker = if used { "*" } else { " " };
        println!("{marker} {file_name} ({})", format_bytes(size));
    }
    println!("total: {}", format_bytes(total_bytes));
    println!("*: referenced by current lockfile");
    Ok(())
}

fn cmd_cache_stats(root: &Path, json: bool) -> Result<()> {
    let cache_root = cache_root_path()?;
    let lock = load_lockfile_optional(root)?;
    let stats = collect_cache_stats(lock.as_ref(), &cache_root)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&stats).context("failed to encode cache stats json")?
        );
        return Ok(());
    }

    println!("cache: {}", cache_root.display());
    println!("files: {}", stats.file_count);
    println!("total: {}", format_bytes(stats.total_bytes));
    println!(
        "referenced: {} ({})",
        stats.referenced_files,
        format_bytes(stats.referenced_bytes)
    );
    println!(
        "unreferenced: {} ({})",
        stats.unreferenced_files,
        format_bytes(stats.unreferenced_bytes)
    );
    Ok(())
}

fn cmd_cache_verify(root: &Path, repair: bool) -> Result<()> {
    let cache_root = cache_root_path()?;
    let lock = load_lockfile_optional(root)?;
    let report = verify_cache_entries(lock.as_ref(), &cache_root, repair)?;
    println!("cache verify: {}", cache_root.display());
    println!(
        "checked={}, valid={}, invalid={}, missing={}, repaired={}, skipped={}",
        report.checked,
        report.valid,
        report.invalid,
        report.missing,
        report.repaired,
        report.skipped
    );
    if report.invalid > 0 && !repair {
        bail!(
            "cache verify detected {} invalid entries (rerun with --repair to remove them)",
            report.invalid
        );
    }
    if report.missing > 0 {
        bail!(
            "cache verify detected {} missing lockfile cache entries",
            report.missing
        );
    }
    Ok(())
}

fn cmd_cache_clean(root: &Path) -> Result<()> {
    let cache_root = cache_root_path()?;
    if !cache_root.exists() {
        println!("cache directory does not exist: {}", cache_root.display());
        return Ok(());
    }

    let lock = load_lockfile_optional(root)?;
    let Some(lock) = lock.as_ref() else {
        println!("lockfile not found, skip clean (use `mineconda cache purge` to clear all cache)");
        return Ok(());
    };

    let used_paths = expected_cache_paths(Some(lock), &cache_root);
    let mut removed = 0usize;
    let mut kept = 0usize;
    let mut freed = 0u64;
    for entry in fs::read_dir(&cache_root)
        .with_context(|| format!("failed to read {}", cache_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if used_paths.contains(&path) {
            kept += 1;
            continue;
        }
        let size = entry.metadata()?.len();
        fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
        removed += 1;
        freed += size;
    }

    println!(
        "cache clean: removed={}, kept={}, freed={}",
        removed,
        kept,
        format_bytes(freed)
    );
    Ok(())
}

fn cmd_cache_purge() -> Result<()> {
    let cache_root = cache_root_path()?;
    if cache_root.exists() {
        fs::remove_dir_all(&cache_root)
            .with_context(|| format!("failed to remove {}", cache_root.display()))?;
    }
    fs::create_dir_all(&cache_root)
        .with_context(|| format!("failed to recreate {}", cache_root.display()))?;
    println!("cache purged: {}", cache_root.display());
    Ok(())
}

fn cmd_cache_remote_prune(
    root: &Path,
    s3: bool,
    max_age_days: u64,
    prefix: Option<String>,
    dry_run: bool,
) -> Result<()> {
    if !s3 {
        bail!("remote prune currently requires --s3");
    }
    let manifest = load_manifest(root)?;
    let cache = manifest
        .cache
        .s3
        .as_ref()
        .context("cache remote-prune requires [cache.s3] in mineconda.toml")?;
    let report = remote_prune_s3_cache(
        cache,
        &RemotePruneRequest {
            max_age_days,
            prefix,
            dry_run,
        },
    )?;
    println!(
        "remote prune: listed={}, candidates={}, deleted={}, retained={}, dry_run={}",
        report.listed, report.candidates, report.deleted, report.retained, dry_run
    );
    Ok(())
}

fn expected_cache_paths(lock: Option<&Lockfile>, cache_root: &Path) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    let Some(lock) = lock else {
        return out;
    };

    for package in &lock.packages {
        out.insert(cache_path_for_package_in(cache_root, package));
    }
    out
}

fn cmd_run(
    root: &Path,
    args: RunCommandArgs,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<()> {
    let RunCommandArgs {
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
    } = args;

    let manifest = load_manifest_optional(root)?;
    let defaults = manifest
        .as_ref()
        .map(|m| m.server.clone())
        .unwrap_or_else(ServerProfile::default);
    let runtime = manifest.as_ref().map(|m| m.runtime.clone());
    let loader_hint = manifest
        .as_ref()
        .map(|manifest| to_run_loader_hint(manifest.project.loader.kind));
    let package_paths = if let Some(manifest) = manifest.as_ref() {
        let active_groups =
            activation_groups_with_profiles(manifest, workspace, &groups, all_groups, profiles)?;
        if manifest.groups.is_empty() && active_groups.len() == 1 {
            None
        } else {
            let lock = load_lockfile_required(root)?;
            ensure_lock_covers_groups(manifest, &lock, &active_groups)?;
            Some(
                filtered_lockfile(&lock, &active_groups)
                    .packages
                    .into_iter()
                    .map(|package| package.install_path_or_default())
                    .collect(),
            )
        }
    } else if !groups.is_empty() || all_groups || !profiles.is_empty() {
        bail!("group/profile filters require a manifest");
    } else {
        None
    };
    let args = if jvm_args.is_empty() {
        defaults.jvm_args.clone()
    } else {
        jvm_args
    };
    let java_bin = resolve_java_for_run(java, &defaults, runtime.as_ref())?;
    let mode = mode.to_core();
    let launcher_jar = launcher_jar.map(|path| {
        if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
    });
    let server_jar = server_jar.map(|path| {
        if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
    });

    let (client_launcher_jar, server_launcher_jar) = match mode {
        RunMode::Client => (launcher_jar, None),
        RunMode::Server => (None, server_jar.or(launcher_jar)),
        RunMode::Both => (launcher_jar, server_jar),
    };

    let request = RunRequest {
        root: root.to_path_buf(),
        java_bin,
        memory: memory.unwrap_or(defaults.memory),
        dry_run,
        extra_jvm_args: args,
        username,
        instance_name: instance,
        mode,
        loader_hint,
        client_launcher_jar,
        server_launcher_jar,
        package_paths,
    };
    run_game_instance(&request)?;
    Ok(())
}

fn cmd_export(
    root: &Path,
    format: ExportArg,
    output: PathBuf,
    groups: Vec<String>,
    all_groups: bool,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<()> {
    let manifest = load_manifest(root)?;
    let lock = load_lockfile_required(root)?;
    let active_groups =
        activation_groups_with_profiles(&manifest, workspace, &groups, all_groups, profiles)?;
    ensure_lock_covers_groups(&manifest, &lock, &active_groups)?;
    let mut manifest = filtered_manifest_for_export(&manifest, &active_groups);
    let mut lock = filtered_lockfile(&lock, &active_groups);
    if matches!(format, ExportArg::Curseforge | ExportArg::Multimc) {
        eprintln!(
            "warning: `{}` export is compatibility-oriented and not part of the stable import/export baseline; validate it with your target launcher",
            format.as_str()
        );
    }
    let resolved_loader_version = resolve_loader_version(
        &manifest.project.minecraft,
        manifest.project.loader.kind,
        &manifest.project.loader.version,
    )
    .context(
        "failed to resolve project loader version for export (pin loader version to avoid network lookup)",
    )?;
    if !manifest
        .project
        .loader
        .version
        .eq_ignore_ascii_case(&resolved_loader_version)
    {
        manifest.project.loader.version = resolved_loader_version.clone();
        lock.metadata.loader.version = resolved_loader_version.clone();
        println!("resolved loader version for export: {resolved_loader_version}");
    }
    let output = if output.is_absolute() {
        output
    } else {
        root.join(output)
    };

    let file = export_pack(
        &manifest,
        &lock,
        &ExportRequest {
            output,
            format: format.to_core(),
            project_root: Some(root.to_path_buf()),
        },
    )?;

    println!("exported {}", file.display());
    Ok(())
}

fn cmd_import(
    root: &Path,
    input: String,
    format: ImportFormatArg,
    side: ImportSideArg,
    force: bool,
) -> Result<()> {
    fs::create_dir_all(root)
        .with_context(|| format!("failed to create root {}", root.display()))?;

    let manifest_out = manifest_path(root);
    if manifest_out.exists() && !force {
        bail!(
            "manifest already exists at {} (use --force to overwrite)",
            manifest_out.display()
        );
    }

    let lock_out = lockfile_path(root);
    if lock_out.exists() && !force {
        bail!(
            "lockfile already exists at {} (use --force to overwrite)",
            lock_out.display()
        );
    }

    init_modpack_layout(root)?;

    let prepared_input = prepare_import_input(input.as_str())?;
    let detected = format
        .to_core()
        .unwrap_or(detect_pack_format(&prepared_input.path)?);
    let imported = import_pack_with_format(
        &ImportRequest {
            input: prepared_input.path.clone(),
            side: side.to_core(),
        },
        detected,
    )?;

    write_manifest(&manifest_out, &imported.manifest)
        .with_context(|| format!("failed to write {}", manifest_out.display()))?;
    write_lockfile(&lock_out, &imported.lockfile)
        .with_context(|| format!("failed to write {}", lock_out.display()))?;
    let overrides = write_import_overrides(root, imported.overrides.as_slice())?;

    println!(
        "imported {} [{}]: mods={}, packages={}, overrides={}",
        input,
        detected.as_str(),
        imported.manifest.mods.len(),
        imported.lockfile.packages.len(),
        overrides
    );
    println!("wrote {}", manifest_out.display());
    println!("wrote {}", lock_out.display());
    Ok(())
}

struct PreparedImportInput {
    path: PathBuf,
    temp_path: Option<PathBuf>,
}

impl Drop for PreparedImportInput {
    fn drop(&mut self) {
        if let Some(path) = self.temp_path.as_ref() {
            let _ = fs::remove_file(path);
        }
    }
}

fn prepare_import_input(input: &str) -> Result<PreparedImportInput> {
    if is_http_url(input) {
        let client = build_import_http_client()?;
        let response = client
            .get(input)
            .send()
            .with_context(|| format!("failed to download import archive {input}"))?
            .error_for_status()
            .with_context(|| format!("import archive request failed for {input}"))?;
        let bytes = response
            .bytes()
            .with_context(|| format!("failed to read import archive body from {input}"))?;
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before UNIX_EPOCH")?
            .as_nanos();
        let path = env::temp_dir().join(format!("mineconda-import-{unique}.zip"));
        fs::write(&path, bytes.as_ref())
            .with_context(|| format!("failed to write downloaded archive {}", path.display()))?;
        return Ok(PreparedImportInput {
            path: path.clone(),
            temp_path: Some(path),
        });
    }

    let path = PathBuf::from(input);
    let path = if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .context("failed to read current working directory")?
            .join(path)
    };

    Ok(PreparedImportInput {
        path,
        temp_path: None,
    })
}

fn build_import_http_client() -> Result<Client> {
    let mut builder = Client::builder()
        .user_agent(mineconda_core::http_user_agent())
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(120));

    if env::var_os("MINECONDA_NO_PROXY")
        .map(|value| value != "0")
        .unwrap_or(false)
    {
        builder = builder.no_proxy();
    }

    builder
        .build()
        .context("failed to build HTTP client for import")
}

fn is_http_url(input: &str) -> bool {
    input.starts_with("https://") || input.starts_with("http://")
}

fn package_install_target_path(root: &Path, package: &LockedPackage) -> PathBuf {
    root.join(package.install_path_or_default())
}

fn override_scope_prefix(scope: OverrideScope) -> &'static str {
    match scope {
        OverrideScope::Common => "overrides",
        OverrideScope::Client => "client-overrides",
        OverrideScope::Server => "server-overrides",
    }
}

fn write_import_overrides(
    root: &Path,
    overrides: &[mineconda_export::ImportedOverrideFile],
) -> Result<usize> {
    let mut written = 0usize;
    for entry in overrides {
        let target = root.join(&entry.relative_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(&target, &entry.bytes)
            .with_context(|| format!("failed to write {}", target.display()))?;

        let scoped_target = root
            .join(override_scope_prefix(entry.scope))
            .join(&entry.relative_path);
        if let Some(parent) = scoped_target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(&scoped_target, &entry.bytes)
            .with_context(|| format!("failed to write {}", scoped_target.display()))?;
        written += 1;
    }
    Ok(written)
}

fn cmd_env(root: &Path, command: EnvCommands, scope: &ScopeArgs) -> Result<()> {
    if matches!(command, EnvCommands::List)
        && load_workspace_optional(root)?.is_some()
        && scope.all_members
    {
        return cmd_env_list_workspace(root);
    }

    let target = resolve_project_target(root, scope)?;
    match command {
        EnvCommands::Install {
            java,
            provider,
            force,
            use_for_project,
        } => cmd_env_install(&target.root, java, provider, force, use_for_project),
        EnvCommands::Use { java, provider } => cmd_env_use(&target.root, java, provider),
        EnvCommands::List => cmd_env_list(&target.root),
        EnvCommands::Which => cmd_env_which(&target.root),
    }
}

fn cmd_env_list_workspace(root: &Path) -> Result<()> {
    let workspace = load_workspace_required(root)?;
    for member in workspace_members(root, &workspace)? {
        let runtime = load_manifest_optional(&member.root)?
            .map(|manifest| {
                format!(
                    "{} ({})",
                    manifest.runtime.java,
                    manifest.runtime.provider.as_str()
                )
            })
            .unwrap_or_else(|| "unconfigured".to_string());
        println!("{}\t{}", member.name, runtime);
    }
    Ok(())
}

fn cmd_env_install(
    root: &Path,
    java: String,
    provider: JavaProviderArg,
    force: bool,
    use_for_project: bool,
) -> Result<()> {
    let provider = provider.to_core();
    let java_bin = ensure_java_runtime(&java, provider, force)?;
    println!(
        "installed java {} ({}) -> {}",
        java,
        provider.as_str(),
        java_bin.display()
    );

    if use_for_project {
        let mut manifest = load_manifest(root)?;
        manifest.runtime.java = java.clone();
        manifest.runtime.provider = provider;
        let path = manifest_path(root);
        write_manifest(&path, &manifest)?;
        println!(
            "project runtime pinned to {} ({}) in {}",
            java,
            provider.as_str(),
            path.display()
        );
    }

    Ok(())
}

fn cmd_env_use(root: &Path, java: String, provider: JavaProviderArg) -> Result<()> {
    let provider = provider.to_core();
    let java_bin = resolve_java_binary(&java, provider, true)?;
    let mut manifest = load_manifest(root)?;
    manifest.runtime.java = java.clone();
    manifest.runtime.provider = provider;

    let path = manifest_path(root);
    write_manifest(&path, &manifest)?;
    println!(
        "active runtime: java {} ({}) -> {}",
        java,
        provider.as_str(),
        java_bin.display()
    );
    println!("updated {}", path.display());
    Ok(())
}

fn cmd_env_list(root: &Path) -> Result<()> {
    let active = load_manifest_optional(root)?
        .map(|m| (m.runtime.java, m.runtime.provider))
        .unwrap_or_else(|| ("".to_string(), JavaProvider::Temurin));

    let runtimes = list_java_runtimes()?;
    if runtimes.is_empty() {
        println!("no managed java runtimes installed");
        return Ok(());
    }

    for runtime in runtimes {
        let marker = if active.0 == runtime.version && active.1 == runtime.provider {
            "*"
        } else {
            " "
        };
        println!(
            "{} java {} ({}) -> {}",
            marker,
            runtime.version,
            runtime.provider.as_str(),
            runtime.java_bin.display()
        );
    }

    Ok(())
}

fn cmd_env_which(root: &Path) -> Result<()> {
    let manifest = load_manifest(root)?;
    let runtime = manifest.runtime;
    match find_java_runtime(&runtime.java, runtime.provider)? {
        Some(path) => {
            println!(
                "java {} ({}) -> {}",
                runtime.java,
                runtime.provider.as_str(),
                path.display()
            );
            Ok(())
        }
        None => bail!(
            "java {} ({}) is not installed. run `mineconda env install {}`",
            runtime.java,
            runtime.provider.as_str(),
            runtime.java
        ),
    }
}

fn validate_manifest_groups(manifest: &Manifest) -> Result<()> {
    for group in manifest.groups.0.keys() {
        if !is_valid_group_name(group) {
            bail!("manifest contains invalid group name `{group}`");
        }
    }
    validate_manifest_profiles(manifest)?;
    Ok(())
}

fn load_manifest(root: &Path) -> Result<Manifest> {
    let path = manifest_path(root);
    let manifest =
        read_manifest(&path).with_context(|| format!("failed to read {}", path.display()))?;
    validate_manifest_groups(&manifest)?;
    Ok(manifest)
}

fn load_manifest_optional(root: &Path) -> Result<Option<Manifest>> {
    let path = manifest_path(root);
    if !path.exists() {
        return Ok(None);
    }
    let manifest =
        read_manifest(&path).with_context(|| format!("failed to read {}", path.display()))?;
    validate_manifest_groups(&manifest)?;
    Ok(Some(manifest))
}

fn load_lockfile_optional(root: &Path) -> Result<Option<Lockfile>> {
    let path = lockfile_path(root);
    if !path.exists() {
        return Ok(None);
    }
    let lock =
        read_lockfile(&path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(Some(lock))
}

fn load_lockfile_required(root: &Path) -> Result<Lockfile> {
    load_lockfile_optional(root)?.context("lockfile not found, run `mineconda lock` first")
}

fn write_lock_from_manifest(
    root: &Path,
    manifest: &Manifest,
    upgrade: bool,
    groups: BTreeSet<String>,
) -> Result<()> {
    let old_lock = load_lockfile_optional(root)?;
    let output = resolve_lockfile(
        manifest,
        old_lock.as_ref(),
        &ResolveRequest { upgrade, groups },
    )?;
    let path = lockfile_path(root);
    write_lockfile(&path, &output.lockfile)
        .with_context(|| format!("failed to write {}", path.display()))?;

    println!(
        "lock updated: install={}, remove={}, unchanged={}",
        output.plan.install.len(),
        output.plan.remove.len(),
        output.plan.unchanged.len()
    );
    Ok(())
}

fn resolve_java_for_run(
    java_override: Option<String>,
    server: &ServerProfile,
    runtime: Option<&RuntimeProfile>,
) -> Result<String> {
    if let Some(java) = java_override {
        return Ok(java);
    }

    if server.java != "java" {
        return Ok(server.java.clone());
    }

    if let Some(runtime) = runtime {
        let java_bin = resolve_java_binary(&runtime.java, runtime.provider, runtime.auto_install)?;
        return Ok(java_bin.display().to_string());
    }

    Ok("java".to_string())
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use clap::Parser;
    use mineconda_core::{
        DEFAULT_GROUP_NAME, LoaderKind, LoaderSpec, LockMetadata, LockedDependency,
        LockedDependencyKind, LockedPackage, Lockfile, Manifest, ModSide, ModSource, ModSpec,
        ProjectSection, RuntimeProfile, ServerProfile, WorkspaceConfig, lockfile_path,
        manifest_path, workspace_path, write_lockfile, write_manifest, write_workspace,
    };
    use mineconda_resolver::{ResolveRequest, SearchResult, SearchSource, resolve_lockfile};
    use serde_json::Value;

    use super::{
        Cli, DoctorLevel, LockDiffKind, ProjectSelection, ScopeArgs, SyncCommandArgs,
        TreeCommandArgs, WhyCommandArgs, activation_groups_with_profiles, build_lock_check_report,
        build_lock_diff_json_report, build_status_json_report, build_sync_check_report,
        build_tree_json_report, build_why_json_report, collect_s3_doctor_findings,
        compute_lock_diff_entries, display_width, extract_curseforge_project_id,
        extract_modrinth_slug, lock_package_matches_request, render_forward_dependency_forest,
        render_lock_diff_entry, render_lock_diff_json_report, render_reverse_dependency_tree,
        render_status_json_report, render_why_report, resolve_java_for_run, resolve_project_target,
        resolve_search_install_target, truncate_visual, wrap_visual,
    };

    #[test]
    fn wrap_visual_keeps_cjk_line_width() {
        let lines = wrap_visual("这是一个中文简介用于测试终端宽度自动换行效果", 12);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|line| display_width(line) <= 12));
    }

    #[test]
    fn wrap_visual_keeps_emoji_line_width() {
        let lines = wrap_visual("emoji 😀 shader pack loader for minecraft", 14);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|line| display_width(line) <= 14));
    }

    #[test]
    fn truncate_visual_respects_display_width() {
        let value = truncate_visual("中文Minecraft模组管理器", 10);
        assert!(value.ends_with('…'));
        assert!(display_width(&value) <= 10);
    }

    #[test]
    fn extract_modrinth_slug_works() {
        let slug = extract_modrinth_slug("https://modrinth.com/mod/sodium?foo=bar");
        assert_eq!(slug.as_deref(), Some("sodium"));
    }

    #[test]
    fn extract_curseforge_project_id_works() {
        let id = extract_curseforge_project_id("https://www.curseforge.com/projects/394468");
        assert_eq!(id.as_deref(), Some("394468"));
    }

    #[test]
    fn resolve_install_target_prefers_modrinth_for_mcmod_item() {
        let item = SearchResult {
            id: "123".to_string(),
            slug: "123".to_string(),
            title: "Sodium".to_string(),
            summary: "test".to_string(),
            source: SearchSource::Mcmod,
            downloads: None,
            url: "https://www.mcmod.cn/class/123.html".to_string(),
            dependencies: Vec::new(),
            supported_side: None,
            source_homepage: Some("https://www.mcmod.cn/class/123.html".to_string()),
            linked_modrinth_url: Some("https://modrinth.com/mod/sodium".to_string()),
            linked_curseforge_url: Some("https://www.curseforge.com/projects/394468".to_string()),
            linked_github_url: None,
        };
        let (id, source) = resolve_search_install_target(&item).expect("resolve target");
        assert_eq!(id, "sodium");
        assert_eq!(source, ModSource::Modrinth);
    }

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

    #[test]
    fn resolve_java_for_run_prefers_explicit_override() {
        let server = ServerProfile::default();
        let runtime = RuntimeProfile::default();
        let java =
            resolve_java_for_run(Some("custom-java".to_string()), &server, Some(&runtime)).unwrap();
        assert_eq!(java, "custom-java");
    }

    #[test]
    fn resolve_java_for_run_prefers_server_java_before_runtime() {
        let server = ServerProfile {
            java: "/opt/java/bin/java".to_string(),
            ..ServerProfile::default()
        };
        let runtime = RuntimeProfile::default();
        let java = resolve_java_for_run(None, &server, Some(&runtime)).unwrap();
        assert_eq!(java, "/opt/java/bin/java");
    }

    fn test_manifest(mods: Vec<ModSpec>) -> Manifest {
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

    fn test_lockfile(packages: Vec<LockedPackage>) -> Lockfile {
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

    fn required_dependency(id: &str) -> LockedDependency {
        LockedDependency {
            source: ModSource::Modrinth,
            id: id.to_string(),
            kind: LockedDependencyKind::Required,
            constraint: None,
        }
    }

    fn incompatible_dependency(id: &str) -> LockedDependency {
        LockedDependency {
            source: ModSource::Modrinth,
            id: id.to_string(),
            kind: LockedDependencyKind::Incompatible,
            constraint: None,
        }
    }

    #[test]
    fn activation_groups_include_default_plus_requested_extras() {
        let manifest = test_manifest_with_client_group();
        let groups = super::activation_groups(&manifest, &["client".to_string()], false)
            .expect("active groups");
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
        let groups =
            super::edit_groups(&manifest, &["client".to_string()], false).expect("edit groups");
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

    fn test_package(id: &str, version: &str, dependencies: Vec<LockedDependency>) -> LockedPackage {
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

    struct TempProject {
        path: PathBuf,
    }

    impl TempProject {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time")
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("mineconda-{name}-{}-{unique}", std::process::id()));
            fs::create_dir_all(&path).expect("temp project directory");
            Self { path }
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn cli_parses_lock_diff_subcommand() {
        let cli = Cli::try_parse_from(["mineconda", "lock", "diff"]).expect("cli should parse");
        assert!(matches!(
            cli.command,
            super::Commands::Lock {
                command: Some(super::LockCommands::Diff { json: false }),
                ..
            }
        ));
    }

    #[test]
    fn cli_parses_json_flags_for_lock_diff_and_status() {
        let cli = Cli::try_parse_from(["mineconda", "lock", "diff", "--json"])
            .expect("lock diff json should parse");
        assert!(matches!(
            cli.command,
            super::Commands::Lock {
                command: Some(super::LockCommands::Diff { json: true }),
                ..
            }
        ));

        let cli =
            Cli::try_parse_from(["mineconda", "status", "--json"]).expect("status json parse");
        assert!(matches!(
            cli.command,
            super::Commands::Status { json: true, .. }
        ));
    }

    #[test]
    fn cli_parses_check_flags_for_lock_and_sync() {
        let cli = Cli::try_parse_from(["mineconda", "lock", "--check"]).expect("lock check parse");
        assert!(matches!(
            cli.command,
            super::Commands::Lock {
                command: None,
                check: true,
                ..
            }
        ));

        let cli = Cli::try_parse_from(["mineconda", "sync", "--check"]).expect("sync check parse");
        assert!(matches!(
            cli.command,
            super::Commands::Sync { check: true, .. }
        ));
    }

    #[test]
    fn cli_parses_workspace_profile_and_json_flags() {
        let cli = Cli::try_parse_from([
            "mineconda",
            "--member",
            "packs/client",
            "--profile",
            "client-dev",
            "ls",
            "--json",
        ])
        .expect("ls json parse");
        assert_eq!(cli.member.as_deref(), Some("packs/client"));
        assert_eq!(cli.profiles, vec!["client-dev".to_string()]);
        assert!(matches!(
            cli.command,
            super::Commands::Ls { json: true, .. }
        ));

        let cli = Cli::try_parse_from(["mineconda", "workspace", "add", "packs/client"])
            .expect("workspace add parse");
        assert!(matches!(
            cli.command,
            super::Commands::Workspace {
                command: super::WorkspaceCommands::Add { .. }
            }
        ));

        let cli = Cli::try_parse_from([
            "mineconda",
            "--workspace",
            "profile",
            "add",
            "client-dev",
            "--group",
            "client",
        ])
        .expect("profile add parse");
        assert!(cli.workspace);
        assert!(matches!(
            cli.command,
            super::Commands::Profile {
                command: super::ProfileCommands::Add { .. }
            }
        ));
    }

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
    fn render_lock_diff_entry_formats_group_changes() {
        let line = render_lock_diff_entry(&super::LockDiffEntry {
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

        let status = build_status_json_report(&project.path, Vec::new(), false, &[], None)
            .expect("status report");
        assert_eq!(status.summary.state, "clean");

        let diff = build_lock_diff_json_report(&project.path, false, Vec::new(), false, &[], None)
            .expect("lock diff report")
            .expect("lock diff should succeed");
        assert!(diff.entries.is_empty());
    }

    #[test]
    fn sync_check_report_detects_missing_and_installed_packages() {
        let project = TempProject::new("sync-check-installed");
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

    #[test]
    fn lock_diff_json_report_serializes_expected_shape() {
        let report = super::LockDiffJsonReport {
            command: "lock-diff",
            groups: vec![DEFAULT_GROUP_NAME.to_string(), "client".to_string()],
            summary: super::LockDiffJsonSummary {
                install: 1,
                remove: 0,
                unchanged: 2,
                changes: 1,
            },
            entries: vec![super::LockDiffJsonEntry {
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
        let report = super::StatusJsonReport {
            command: "status",
            groups: vec![DEFAULT_GROUP_NAME.to_string()],
            summary: super::StatusJsonSummary {
                state: "clean",
                exit_code: 0,
            },
            manifest: super::StatusJsonManifest {
                exists: true,
                path: "/tmp/mineconda.toml".to_string(),
                roots: Some(1),
                named_groups: Some(0),
            },
            lockfile: super::StatusJsonLockfile {
                exists: true,
                path: "/tmp/mineconda.lock".to_string(),
                packages: Some(1),
                dependency_graph: Some(true),
                group_metadata: Some(true),
            },
            checks: super::StatusJsonChecks {
                project_metadata: "aligned",
                group_coverage: "ok",
                resolution: "up_to_date",
                sync: super::StatusJsonSync {
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

    #[test]
    fn lock_diff_text_report_renders_from_json_report() {
        let report = super::LockDiffJsonReport {
            command: "lock-diff",
            groups: vec![DEFAULT_GROUP_NAME.to_string()],
            summary: super::LockDiffJsonSummary {
                install: 1,
                remove: 0,
                unchanged: 0,
                changes: 1,
            },
            entries: vec![super::LockDiffJsonEntry {
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

    #[test]
    fn forward_tree_renders_nested_dependencies() {
        let alpha = test_package("alpha", "1.0.0", vec![required_dependency("beta")]);
        let beta = test_package("beta", "1.1.0", vec![required_dependency("gamma")]);
        let gamma = test_package("gamma", "1.2.0", Vec::new());
        let lock = test_lockfile(vec![alpha.clone(), beta, gamma]);
        let graph = super::LockGraph::from_lock(&lock);

        let output = render_forward_dependency_forest(&graph, &[&alpha]);
        assert_eq!(
            output,
            concat!(
                "alpha [modrinth] 1.0.0\n",
                "`-- beta [modrinth] 1.1.0 (required)\n",
                "    `-- gamma [modrinth] 1.2.0 (required)\n",
            )
        );
    }

    #[test]
    fn reverse_tree_renders_nested_dependents() {
        let alpha = test_package("alpha", "1.0.0", vec![required_dependency("beta")]);
        let beta = test_package("beta", "1.1.0", vec![required_dependency("gamma")]);
        let gamma = test_package("gamma", "1.2.0", Vec::new());
        let lock = test_lockfile(vec![alpha, beta.clone(), gamma.clone()]);
        let graph = super::LockGraph::from_lock(&lock);
        let groups = BTreeSet::from([DEFAULT_GROUP_NAME.to_string()]);

        let output = render_reverse_dependency_tree(&graph, &gamma, &groups);
        assert_eq!(
            output,
            concat!(
                "gamma [modrinth] 1.2.0\n",
                "`-- beta [modrinth] 1.1.0 (depended on via required)\n",
                "    `-- alpha [modrinth] 1.0.0 (depended on via required)\n",
            )
        );
    }

    #[test]
    fn tree_json_report_contains_nodes_and_edges() {
        let project = TempProject::new("tree-json");
        let manifest: Manifest = toml::from_str(
            r#"
[project]
name = "pack"
minecraft = "1.21.1"

[project.loader]
kind = "neo-forge"
version = "latest"

[[mods]]
id = "alpha"
source = "modrinth"
version = "1.0.0"
side = "both"
"#,
        )
        .expect("manifest");
        let lock = test_lockfile(vec![
            test_package("alpha", "1.0.0", vec![required_dependency("beta")]),
            test_package("beta", "1.1.0", vec![required_dependency("gamma")]),
            test_package("gamma", "1.2.0", Vec::new()),
        ]);
        write_manifest(&manifest_path(&project.path), &manifest).expect("write manifest");
        write_lockfile(&lockfile_path(&project.path), &lock).expect("write lock");

        let report = build_tree_json_report(
            &project.path,
            &TreeCommandArgs {
                id: None,
                invert: None,
                all: false,
                source: None,
                json: true,
            },
            ProjectSelection {
                groups: &[],
                all_groups: false,
                profiles: &[],
                workspace: None,
                member_name: None,
            },
        )
        .expect("tree report");

        assert_eq!(report.command, "tree");
        assert_eq!(report.mode, "roots");
        assert_eq!(report.direction, "forward");
        assert_eq!(report.roots, vec!["alpha@modrinth".to_string()]);
        assert_eq!(report.nodes.len(), 3);
        assert!(report.edges.iter().any(|edge| {
            edge.from == "alpha@modrinth" && edge.to == "beta@modrinth" && edge.kind == "required"
        }));
    }

    #[test]
    fn why_json_report_contains_transitive_path() {
        let project = TempProject::new("why-json");
        let manifest: Manifest = toml::from_str(
            r#"
[project]
name = "pack"
minecraft = "1.21.1"

[project.loader]
kind = "neo-forge"
version = "latest"

[[mods]]
id = "alpha"
source = "modrinth"
version = "1.0.0"
side = "both"
"#,
        )
        .expect("manifest");
        let lock = test_lockfile(vec![
            test_package("alpha", "1.0.0", vec![required_dependency("beta")]),
            test_package("beta", "1.1.0", vec![required_dependency("gamma")]),
            test_package("gamma", "1.2.0", Vec::new()),
        ]);
        write_manifest(&manifest_path(&project.path), &manifest).expect("write manifest");
        write_lockfile(&lockfile_path(&project.path), &lock).expect("write lock");

        let report = build_why_json_report(
            &project.path,
            &WhyCommandArgs {
                id: "beta".to_string(),
                source: None,
                json: true,
            },
            ProjectSelection {
                groups: &[],
                all_groups: false,
                profiles: &[],
                workspace: None,
                member_name: None,
            },
        )
        .expect("why report");

        assert_eq!(report.command, "why");
        assert_eq!(report.reason, "transitive");
        assert!(!report.direct);
        assert_eq!(report.target.id, "beta");
        assert_eq!(report.paths.len(), 1);
        assert_eq!(
            report.paths[0]
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
    }

    #[test]
    fn why_report_marks_direct_dependency() {
        let alpha = test_package("alpha", "1.0.0", vec![required_dependency("beta")]);
        let lock = test_lockfile(vec![
            alpha.clone(),
            test_package("beta", "1.1.0", Vec::new()),
        ]);
        let graph = super::LockGraph::from_lock(&lock);

        let output = render_why_report(&graph, &[&alpha], &alpha).expect("direct why report");
        assert_eq!(
            output,
            "alpha [modrinth] 1.0.0 is a direct dependency\nalpha [modrinth] 1.0.0\n"
        );
    }

    #[test]
    fn why_report_lists_shortest_transitive_paths() {
        let alpha = test_package("alpha", "1.0.0", vec![required_dependency("beta")]);
        let beta = test_package("beta", "1.1.0", vec![required_dependency("gamma")]);
        let delta = test_package("delta", "2.0.0", vec![required_dependency("gamma")]);
        let gamma = test_package("gamma", "1.2.0", Vec::new());
        let manifest = test_manifest(vec![
            ModSpec::new(
                "alpha".to_string(),
                ModSource::Modrinth,
                "latest".to_string(),
                ModSide::Both,
            ),
            ModSpec::new(
                "delta".to_string(),
                ModSource::Modrinth,
                "latest".to_string(),
                ModSide::Both,
            ),
        ]);
        let lock = test_lockfile(vec![alpha.clone(), beta, delta.clone(), gamma.clone()]);
        let graph = super::LockGraph::from_lock(&lock);
        let groups = BTreeSet::from([DEFAULT_GROUP_NAME.to_string()]);
        let roots = super::resolve_manifest_root_packages(&manifest, &lock, &groups)
            .expect("manifest roots");

        let output = render_why_report(&graph, &roots, &gamma).expect("transitive why report");
        assert_eq!(
            output,
            concat!(
                "gamma [modrinth] 1.2.0 is a transitive dependency\n",
                "- delta [modrinth] 2.0.0 -> gamma [modrinth] 1.2.0\n",
            )
        );
    }

    #[test]
    fn why_report_ignores_incompatible_edges() {
        let alpha = test_package("alpha", "1.0.0", vec![incompatible_dependency("gamma")]);
        let gamma = test_package("gamma", "1.2.0", Vec::new());
        let lock = test_lockfile(vec![alpha.clone(), gamma.clone()]);
        let graph = super::LockGraph::from_lock(&lock);

        let error =
            render_why_report(&graph, &[&alpha], &gamma).expect_err("should be unreachable");
        assert!(
            error
                .to_string()
                .contains("gamma [modrinth] 1.2.0 is locked but not reachable from manifest roots")
        );
    }

    #[test]
    fn s3_doctor_findings_remain_non_blocking_for_experimental_config() {
        let manifest: Manifest = toml::from_str(
            r#"
[project]
name = "pack"
minecraft = "1.21.1"

[project.loader]
kind = "neo-forge"
version = "latest"

[[mods]]
id = "iris"
source = "s3"
version = "packs/dev/iris.jar"
side = "both"

[cache.s3]
enabled = true
bucket = ""
auth = "sigv4"
access_key_env = "ACCESS_KEY"
"#,
        )
        .expect("manifest should parse");

        let findings = collect_s3_doctor_findings(&manifest, |_| false);
        assert!(
            findings
                .iter()
                .all(|finding| finding.level != DoctorLevel::Fail)
        );
        assert!(findings.iter().any(
            |finding| finding.title == "s3 source config" && finding.level == DoctorLevel::Warn
        ));
        assert!(findings.iter().any(
            |finding| finding.title == "s3 cache config" && finding.level == DoctorLevel::Warn
        ));
        assert!(
            findings
                .iter()
                .any(|finding| finding.title == "s3 status" && finding.level == DoctorLevel::Ok)
        );
    }

    #[test]
    fn s3_doctor_findings_report_env_presence_and_experimental_status() {
        let manifest: Manifest = toml::from_str(
            r#"
[project]
name = "pack"
minecraft = "1.21.1"

[project.loader]
kind = "neo-forge"
version = "latest"

[sources.s3]
bucket = "mods"
key_prefix = "packs/dev"

[cache.s3]
enabled = true
bucket = "mods-cache"
auth = "auto"
access_key_env = "ACCESS_KEY"
secret_key_env = "SECRET_KEY"
"#,
        )
        .expect("manifest should parse");

        let findings = collect_s3_doctor_findings(&manifest, |name| name == "ACCESS_KEY");
        assert!(
            findings
                .iter()
                .any(|finding| finding.title == "s3 status"
                    && finding.detail.contains("experimental"))
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.detail == "ACCESS_KEY is set")
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.detail == "SECRET_KEY is not set")
        );
    }
}
