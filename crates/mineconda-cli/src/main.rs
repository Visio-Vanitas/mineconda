use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use mineconda_core::{
    JavaProvider, LoaderKind, LockedDependencyKind, LockedPackage, Lockfile, Manifest, ModSide,
    ModSource, ModSpec, RuntimeProfile, S3CacheAuth, S3CacheConfig, S3SourceConfig, ServerProfile,
    lockfile_path, manifest_path, read_lockfile, read_manifest, write_lockfile, write_manifest,
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
        no_lock: bool,
    },
    Remove {
        id: String,
        #[arg(long, value_enum)]
        source: Option<SourceArg>,
        #[arg(long)]
        no_lock: bool,
    },
    Ls {
        #[arg(long)]
        status: bool,
        #[arg(long)]
        info: bool,
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
    },
    Tree {
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        invert: Option<String>,
        #[arg(long, conflicts_with_all = ["id", "invert"])]
        all: bool,
        #[arg(long, value_enum)]
        source: Option<SourceArg>,
    },
    Why {
        id: String,
        #[arg(long, value_enum)]
        source: Option<SourceArg>,
    },
    #[command(visible_alias = "upgrade")]
    Update {
        id: Option<String>,
        #[arg(long, value_enum)]
        source: Option<SourceArg>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        no_lock: bool,
    },
    Pin {
        id: String,
        #[arg(long, value_enum)]
        source: Option<SourceArg>,
        #[arg(long)]
        version: Option<String>,
        #[arg(long)]
        no_lock: bool,
    },
    Lock {
        #[arg(long)]
        upgrade: bool,
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
        locked: bool,
        #[arg(long)]
        frozen: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long, default_value_t = default_sync_jobs())]
        jobs: usize,
        #[arg(long)]
        verbose_cache: bool,
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
    },
    Export {
        #[arg(long, value_enum, default_value_t = ExportArg::Mrpack)]
        format: ExportArg,
        #[arg(long, default_value = "dist/modpack")]
        output: PathBuf,
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = cli.root;
    let no_color = cli.no_color;
    i18n::init(cli.lang.to_preference());

    match cli.command {
        Commands::Init {
            name,
            minecraft,
            loader,
            loader_version,
            bare,
        } => cmd_init(&root, name, minecraft, loader, loader_version, bare)?,
        Commands::Add {
            id,
            source,
            version,
            side,
            no_lock,
        } => cmd_add(&root, id, source, version, side, no_lock)?,
        Commands::Remove {
            id,
            source,
            no_lock,
        } => cmd_remove(&root, &id, source, no_lock)?,
        Commands::Ls { status, info } => cmd_ls(&root, status, info, no_color)?,
        Commands::Search {
            query,
            source,
            limit,
            page,
            non_interactive,
            install_first,
            install_version,
        } => cmd_search(
            &root,
            SearchCommandArgs {
                query,
                source,
                limit,
                page,
                no_color,
                non_interactive,
                install_first,
                install_version,
            },
        )?,
        Commands::Tree {
            id,
            invert,
            all,
            source,
        } => cmd_tree(&root, id, invert, all, source)?,
        Commands::Why { id, source } => cmd_why(&root, &id, source)?,
        Commands::Update {
            id,
            source,
            to,
            no_lock,
        } => cmd_update(&root, id, source, to, no_lock)?,
        Commands::Pin {
            id,
            source,
            version,
            no_lock,
        } => cmd_pin(&root, id, source, version, no_lock)?,
        Commands::Lock { upgrade } => cmd_lock(&root, upgrade)?,
        Commands::Cache { command } => cmd_cache(&root, command)?,
        Commands::Env { command } => cmd_env(&root, command)?,
        Commands::Sync {
            no_prune,
            locked,
            frozen,
            offline,
            jobs,
            verbose_cache,
        } => cmd_sync(
            &root,
            !no_prune,
            locked || frozen,
            offline,
            jobs,
            verbose_cache,
        )?,
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
        } => cmd_run(
            &root,
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
            },
        )?,
        Commands::Export { format, output } => cmd_export(&root, format, output)?,
        Commands::Import {
            input,
            format,
            side,
            force,
        } => cmd_import(&root, input, format, side, force)?,
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
    no_lock: bool,
) -> Result<()> {
    let path = manifest_path(root);
    let mut manifest = load_manifest(root)?;

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
    if let Some(existing) = manifest
        .mods
        .iter_mut()
        .find(|entry| entry.id == id && entry.source == source)
    {
        existing.version = version.clone();
        existing.side = side;
        println!("updated mod {} in {}", id, path.display());
    } else {
        manifest
            .mods
            .push(ModSpec::new(id.clone(), source, version, side));
        println!("added mod {} to {}", id, path.display());
    }

    write_manifest(&path, &manifest)?;
    if !no_lock {
        write_lock_from_manifest(root, &manifest, false)?;
    }
    Ok(())
}

fn cmd_remove(root: &Path, id: &str, source: Option<SourceArg>, no_lock: bool) -> Result<()> {
    let path = manifest_path(root);
    let mut manifest = load_manifest(root)?;
    let source_filter = source.map(SourceArg::to_core);
    let before = manifest.mods.len();
    manifest.mods.retain(|item| {
        if item.id != id {
            return true;
        }
        match source_filter {
            Some(source) => item.source != source,
            None => false,
        }
    });
    write_manifest(&path, &manifest)?;
    let removed = before.saturating_sub(manifest.mods.len());
    println!("removed {removed} matching entries");
    if !no_lock {
        write_lock_from_manifest(root, &manifest, false)?;
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

fn cmd_ls(root: &Path, show_status: bool, show_info: bool, no_color: bool) -> Result<()> {
    let manifest = load_manifest(root)?;
    if manifest.mods.is_empty() {
        println!("manifest has no mods");
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

    println!("📦 mods: {}", manifest.mods.len());
    for (index, spec) in manifest.mods.iter().enumerate() {
        let locked = lock.as_ref().and_then(|item| {
            item.packages
                .iter()
                .find(|pkg| lock_package_matches_spec(pkg, spec))
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

        let resolved = locked.map(|pkg| pkg.version.as_str()).unwrap_or("-");
        let mut line = format!(
            "{:>2}. {} [{}] req={} side={}",
            index + 1,
            spec.id,
            spec.source.as_str(),
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

    Ok(())
}

fn cmd_update(
    root: &Path,
    id: Option<String>,
    source: Option<SourceArg>,
    to: Option<String>,
    no_lock: bool,
) -> Result<()> {
    let mut manifest = load_manifest(root)?;
    let path = manifest_path(root);

    if let Some(id) = id {
        let source_filter = source.map(SourceArg::to_core);
        let target = to.unwrap_or_else(|| "latest".to_string());
        let mut changed = 0usize;

        for spec in &mut manifest.mods {
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

        if changed == 0 {
            bail!("mod `{id}` not found in manifest");
        }

        write_manifest(&path, &manifest)?;
        println!("updated {changed} entries of `{id}` to constraint `{target}`");
        if !no_lock {
            write_lock_from_manifest(root, &manifest, true)?;
        }
        return Ok(());
    }

    if source.is_some() || to.is_some() {
        bail!("`--source` and `--to` require an <id>");
    }

    if no_lock {
        println!("no operation: use `mineconda update <id>` or remove `--no-lock`");
        return Ok(());
    }

    write_lock_from_manifest(root, &manifest, true)?;
    Ok(())
}

fn cmd_pin(
    root: &Path,
    id: String,
    source: Option<SourceArg>,
    version: Option<String>,
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

    let mut changed = 0usize;
    for spec in &mut manifest.mods {
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

    if changed == 0 {
        bail!("mod `{id}` not found in manifest");
    }

    write_manifest(&path, &manifest)?;
    println!("pinned {changed} entries of `{id}` to `{pin_version}`");
    if !no_lock {
        write_lock_from_manifest(root, &manifest, false)?;
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
) -> Result<()> {
    let (id, source) = resolve_search_install_target(item)?;
    let mut manifest = load_manifest(root)?;
    let side = item.supported_side.unwrap_or(ModSide::Both);
    let target_version =
        resolve_install_version_for_search(&id, source, version, minecraft_version, loader)?;

    if let Some(existing) = manifest
        .mods
        .iter_mut()
        .find(|entry| entry.id == id && entry.source == source)
    {
        existing.version = target_version.to_string();
        existing.side = side;
    } else {
        manifest.mods.push(ModSpec::new(
            id.clone(),
            source,
            target_version.to_string(),
            side,
        ));
    }

    let path = manifest_path(root);
    write_manifest(&path, &manifest)
        .with_context(|| format!("failed to write {}", path.display()))?;
    write_lock_from_manifest(root, &manifest, false)?;
    cmd_sync(root, true, false, false, default_sync_jobs(), false)?;

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

fn cmd_tree(
    root: &Path,
    id: Option<String>,
    invert: Option<String>,
    all: bool,
    source: Option<SourceArg>,
) -> Result<()> {
    let manifest = load_manifest(root)?;
    let lock = load_lockfile_required(root)?;
    ensure_lock_dependency_graph(&lock)?;
    let graph = LockGraph::from_lock(&lock);
    let source_filter = source.map(SourceArg::to_core);

    let output = if let Some(query) = invert.as_deref() {
        let target = resolve_locked_package(&lock, query, source_filter)?;
        render_reverse_dependency_tree(&graph, target)
    } else if all {
        let packages = sorted_lock_packages(&lock);
        render_forward_dependency_forest(&graph, &packages)
    } else if let Some(query) = id.as_deref() {
        let target = resolve_locked_package(&lock, query, source_filter)?;
        render_forward_dependency_forest(&graph, &[target])
    } else {
        let roots = resolve_manifest_root_packages(&manifest, &lock)?;
        render_forward_dependency_forest(&graph, &roots)
    };

    print!("{output}");
    Ok(())
}

fn cmd_why(root: &Path, id: &str, source: Option<SourceArg>) -> Result<()> {
    let manifest = load_manifest(root)?;
    let lock = load_lockfile_required(root)?;
    ensure_lock_dependency_graph(&lock)?;
    let graph = LockGraph::from_lock(&lock);
    let roots = resolve_manifest_root_packages(&manifest, &lock)?;
    let target = resolve_locked_package(&lock, id, source.map(SourceArg::to_core))?;
    let output = render_why_report(&graph, &roots, target)?;
    print!("{output}");
    Ok(())
}

fn render_why_report(
    graph: &LockGraph<'_>,
    roots: &[&LockedPackage],
    target: &LockedPackage,
) -> Result<String> {
    let target_key = locked_package_graph_key(target);
    let root_keys: Vec<String> = roots
        .iter()
        .map(|package| locked_package_graph_key(package))
        .collect();

    if root_keys.iter().any(|key| key == &target_key) {
        return Ok(format!(
            "{} is a direct dependency\n{}\n",
            locked_package_display(target),
            locked_package_display(target)
        ));
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

fn render_reverse_dependency_tree(graph: &LockGraph<'_>, root: &LockedPackage) -> String {
    let mut lines = Vec::new();
    let key = locked_package_graph_key(root);
    render_reverse_tree_node(graph, &key, "", true, None, &mut Vec::new(), &mut lines);
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
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

    let label = graph
        .package(key)
        .map(locked_package_display)
        .unwrap_or_else(|| key.to_string());
    let edge_label = incoming
        .map(format_reverse_tree_edge_label)
        .unwrap_or_default();
    lines.push(format!("{prefix}{connector}{label}{edge_label}"));

    path.push(key.to_string());
    let child_prefix = if incoming.is_none() {
        String::new()
    } else if is_last {
        format!("{prefix}    ")
    } else {
        format!("{prefix}|   ")
    };

    let edges = graph.reverse_edges.get(key).cloned().unwrap_or_default();
    for (index, edge) in edges.iter().enumerate() {
        render_reverse_tree_node(
            graph,
            &edge.dependent_key,
            &child_prefix,
            index + 1 == edges.len(),
            Some(edge),
            path,
            lines,
        );
    }
    path.pop();
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
) -> Result<Vec<&'a LockedPackage>> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();

    for spec in &manifest.mods {
        let package = lock
            .packages
            .iter()
            .find(|package| lock_package_matches_spec(package, spec))
            .with_context(|| {
                format!(
                    "manifest entry `{}` [{}] is not present in lockfile; rerun `mineconda lock`",
                    spec.id,
                    spec.source.as_str()
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
) -> Result<&'a LockedPackage> {
    let matches: Vec<&LockedPackage> = lock
        .packages
        .iter()
        .filter(|package| lock_package_matches_request(package, id, source))
        .collect();

    match matches.as_slice() {
        [] => bail!("package `{id}` not found in lockfile"),
        [package] => Ok(*package),
        _ => bail!("multiple lockfile entries match `{id}`, use `--source` to disambiguate"),
    }
}

fn sorted_lock_packages(lock: &Lockfile) -> Vec<&LockedPackage> {
    let mut packages: Vec<&LockedPackage> = lock.packages.iter().collect();
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

fn cmd_lock(root: &Path, upgrade: bool) -> Result<()> {
    let manifest = load_manifest(root)?;
    write_lock_from_manifest(root, &manifest, upgrade)
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
    prune: bool,
    locked: bool,
    offline: bool,
    jobs: usize,
    verbose_cache: bool,
) -> Result<()> {
    let manifest = load_manifest_optional(root)?;
    if jobs == 0 {
        bail!("sync --jobs must be >= 1");
    }
    let mut lock = load_lockfile_required(root)?;
    let report = sync_lockfile(
        &mut lock,
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

fn cmd_run(root: &Path, args: RunCommandArgs) -> Result<()> {
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
    };
    run_game_instance(&request)?;
    Ok(())
}

fn cmd_export(root: &Path, format: ExportArg, output: PathBuf) -> Result<()> {
    let mut manifest = load_manifest(root)?;
    let mut lock = load_lockfile_required(root)?;
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

fn cmd_env(root: &Path, command: EnvCommands) -> Result<()> {
    match command {
        EnvCommands::Install {
            java,
            provider,
            force,
            use_for_project,
        } => cmd_env_install(root, java, provider, force, use_for_project),
        EnvCommands::Use { java, provider } => cmd_env_use(root, java, provider),
        EnvCommands::List => cmd_env_list(root),
        EnvCommands::Which => cmd_env_which(root),
    }
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

fn load_manifest(root: &Path) -> Result<Manifest> {
    let path = manifest_path(root);
    read_manifest(&path).with_context(|| format!("failed to read {}", path.display()))
}

fn load_manifest_optional(root: &Path) -> Result<Option<Manifest>> {
    let path = manifest_path(root);
    if !path.exists() {
        return Ok(None);
    }
    let manifest =
        read_manifest(&path).with_context(|| format!("failed to read {}", path.display()))?;
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

fn write_lock_from_manifest(root: &Path, manifest: &Manifest, upgrade: bool) -> Result<()> {
    let old_lock = load_lockfile_optional(root)?;
    let output = resolve_lockfile(manifest, old_lock.as_ref(), &ResolveRequest { upgrade })?;
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
    use mineconda_core::{
        LoaderKind, LoaderSpec, LockMetadata, LockedDependency, LockedDependencyKind,
        LockedPackage, Lockfile, Manifest, ModSide, ModSource, ModSpec, ProjectSection,
        RuntimeProfile, ServerProfile,
    };
    use mineconda_resolver::{SearchResult, SearchSource};

    use super::{
        DoctorLevel, collect_s3_doctor_findings, display_width, extract_curseforge_project_id,
        extract_modrinth_slug, lock_package_matches_request, render_forward_dependency_forest,
        render_reverse_dependency_tree, render_why_report, resolve_java_for_run,
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
            sources: Default::default(),
            cache: Default::default(),
            server: ServerProfile::default(),
            runtime: RuntimeProfile::default(),
        }
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
            dependencies,
        }
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

        let output = render_reverse_dependency_tree(&graph, &gamma);
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
        let roots =
            super::resolve_manifest_root_packages(&manifest, &lock).expect("manifest roots");

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
