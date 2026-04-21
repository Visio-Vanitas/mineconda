use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use mineconda_core::{JavaProvider, ModSide, ModSource};
use mineconda_export::{ExportFormat, ImportFormat as PackImportFormat, ImportSide};
use mineconda_resolver::SearchSource;
use mineconda_runner::RunMode;

use crate::i18n;

#[derive(Parser, Debug)]
#[command(
    name = "mineconda",
    about = "Minecraft mod package manager inspired by uv"
)]
pub(crate) struct Cli {
    #[arg(long, global = true, default_value = ".")]
    pub(crate) root: PathBuf,
    #[arg(long, global = true)]
    pub(crate) workspace: bool,
    #[arg(long, global = true)]
    pub(crate) member: Option<String>,
    #[arg(long, global = true)]
    pub(crate) all_members: bool,
    #[arg(long = "profile", global = true)]
    pub(crate) profiles: Vec<String>,
    #[arg(long, global = true)]
    pub(crate) no_color: bool,
    #[arg(long, global = true, value_enum, default_value_t = LangArg::Auto)]
    pub(crate) lang: LangArg,
    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Commands {
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
pub(crate) enum EnvCommands {
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
pub(crate) enum GroupCommands {
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
pub(crate) enum ProfileCommands {
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
pub(crate) enum WorkspaceCommands {
    Init { name: String },
    Ls,
    Add { path: String },
    Remove { path: String },
}

#[derive(Subcommand, Debug)]
pub(crate) enum LockCommands {
    Diff {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum CacheCommands {
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
pub(crate) enum LoaderArg {
    Fabric,
    Forge,
    Neoforge,
    Quilt,
}

impl LoaderArg {
    pub(crate) fn to_core(self) -> mineconda_core::LoaderKind {
        match self {
            Self::Fabric => mineconda_core::LoaderKind::Fabric,
            Self::Forge => mineconda_core::LoaderKind::Forge,
            Self::Neoforge => mineconda_core::LoaderKind::NeoForge,
            Self::Quilt => mineconda_core::LoaderKind::Quilt,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum SourceArg {
    Modrinth,
    Curseforge,
    Url,
    Local,
    S3,
}

impl SourceArg {
    pub(crate) fn to_core(self) -> ModSource {
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
pub(crate) enum SideArg {
    Both,
    Client,
    Server,
}

impl SideArg {
    pub(crate) fn to_core(self) -> ModSide {
        match self {
            Self::Both => ModSide::Both,
            Self::Client => ModSide::Client,
            Self::Server => ModSide::Server,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SearchSourceArg {
    Modrinth,
    Curseforge,
    Mcmod,
}

impl SearchSourceArg {
    pub(crate) fn to_core(self) -> SearchSource {
        match self {
            Self::Modrinth => SearchSource::Modrinth,
            Self::Curseforge => SearchSource::Curseforge,
            Self::Mcmod => SearchSource::Mcmod,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ExportArg {
    Curseforge,
    Mrpack,
    Multimc,
    #[value(name = "mods-desc")]
    ModsDesc,
}

impl ExportArg {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Curseforge => "curseforge",
            Self::Mrpack => "mrpack",
            Self::Multimc => "multimc",
            Self::ModsDesc => "mods-desc",
        }
    }

    pub(crate) fn to_core(self) -> ExportFormat {
        match self {
            Self::Curseforge => ExportFormat::CurseforgeZip,
            Self::Mrpack => ExportFormat::Mrpack,
            Self::Multimc => ExportFormat::MultiMcZip,
            Self::ModsDesc => ExportFormat::ModsDescriptionJson,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum JavaProviderArg {
    Temurin,
}

impl JavaProviderArg {
    pub(crate) fn to_core(self) -> JavaProvider {
        match self {
            Self::Temurin => JavaProvider::Temurin,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum RunModeArg {
    Client,
    Server,
    Both,
}

impl RunModeArg {
    pub(crate) fn to_core(self) -> RunMode {
        match self {
            Self::Client => RunMode::Client,
            Self::Server => RunMode::Server,
            Self::Both => RunMode::Both,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ImportSideArg {
    Client,
    Server,
    Both,
}

impl ImportSideArg {
    pub(crate) fn to_core(self) -> ImportSide {
        match self {
            Self::Client => ImportSide::Client,
            Self::Server => ImportSide::Server,
            Self::Both => ImportSide::Both,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ImportFormatArg {
    Auto,
    Mrpack,
}

impl ImportFormatArg {
    pub(crate) fn to_core(self) -> Option<PackImportFormat> {
        match self {
            Self::Auto => None,
            Self::Mrpack => Some(PackImportFormat::Mrpack),
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum LangArg {
    Auto,
    En,
    #[value(name = "zh-cn", alias = "zh")]
    ZhCn,
}

impl LangArg {
    pub(crate) fn to_preference(self) -> i18n::LangPreference {
        match self {
            Self::Auto => i18n::LangPreference::Auto,
            Self::En => i18n::LangPreference::En,
            Self::ZhCn => i18n::LangPreference::ZhCn,
        }
    }
}

pub(crate) fn default_sync_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|value| value.get().saturating_mul(2))
        .unwrap_or(4)
        .clamp(1, 8)
}

#[derive(Debug)]
pub(crate) struct RunCommandArgs {
    pub(crate) dry_run: bool,
    pub(crate) java: Option<String>,
    pub(crate) memory: Option<String>,
    pub(crate) jvm_args: Vec<String>,
    pub(crate) mode: RunModeArg,
    pub(crate) username: String,
    pub(crate) instance: String,
    pub(crate) launcher_jar: Option<PathBuf>,
    pub(crate) server_jar: Option<PathBuf>,
    pub(crate) groups: Vec<String>,
    pub(crate) all_groups: bool,
}

#[derive(Debug)]
pub(crate) struct SearchCommandArgs {
    pub(crate) query: String,
    pub(crate) source: SearchSourceArg,
    pub(crate) limit: usize,
    pub(crate) page: usize,
    pub(crate) no_color: bool,
    pub(crate) non_interactive: bool,
    pub(crate) install_first: bool,
    pub(crate) install_version: Option<String>,
    pub(crate) group: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ScopeArgs {
    pub(crate) workspace: bool,
    pub(crate) member: Option<String>,
    pub(crate) all_members: bool,
    pub(crate) profiles: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct TreeCommandArgs {
    pub(crate) id: Option<String>,
    pub(crate) invert: Option<String>,
    pub(crate) all: bool,
    pub(crate) source: Option<SourceArg>,
    pub(crate) json: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct WhyCommandArgs {
    pub(crate) id: String,
    pub(crate) source: Option<SourceArg>,
    pub(crate) json: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct SyncCommandArgs {
    pub(crate) prune: bool,
    pub(crate) check: bool,
    pub(crate) locked: bool,
    pub(crate) offline: bool,
    pub(crate) jobs: usize,
    pub(crate) verbose_cache: bool,
    pub(crate) groups: Vec<String>,
    pub(crate) all_groups: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_lock_diff_subcommand() {
        let cli = Cli::try_parse_from(["mineconda", "lock", "diff"]).expect("cli should parse");
        assert!(matches!(
            cli.command,
            Commands::Lock {
                command: Some(LockCommands::Diff { json: false }),
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
            Commands::Lock {
                command: Some(LockCommands::Diff { json: true }),
                ..
            }
        ));

        let cli =
            Cli::try_parse_from(["mineconda", "status", "--json"]).expect("status json parse");
        assert!(matches!(cli.command, Commands::Status { json: true, .. }));
    }

    #[test]
    fn cli_parses_check_flags_for_lock_and_sync() {
        let cli = Cli::try_parse_from(["mineconda", "lock", "--check"]).expect("lock check parse");
        assert!(matches!(
            cli.command,
            Commands::Lock {
                command: None,
                check: true,
                ..
            }
        ));

        let cli = Cli::try_parse_from(["mineconda", "sync", "--check"]).expect("sync check parse");
        assert!(matches!(cli.command, Commands::Sync { check: true, .. }));
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
        assert!(matches!(cli.command, Commands::Ls { json: true, .. }));

        let cli = Cli::try_parse_from(["mineconda", "workspace", "add", "packs/client"])
            .expect("workspace add parse");
        assert!(matches!(
            cli.command,
            Commands::Workspace {
                command: WorkspaceCommands::Add { .. }
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
            Commands::Profile {
                command: ProfileCommands::Add { .. }
            }
        ));
    }
}
