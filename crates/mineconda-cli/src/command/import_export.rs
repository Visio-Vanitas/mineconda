use crate::*;

#[derive(Debug, Clone)]
struct ExportExecution {
    compatibility_warning: Option<String>,
    resolved_loader_version: Option<String>,
    file: PathBuf,
    groups: Vec<String>,
    profiles: Vec<String>,
    format: String,
}

#[derive(Debug, Clone)]
struct ImportExecution {
    input: String,
    detected: mineconda_export::ImportFormat,
    side: String,
    force: bool,
    manifest_out: PathBuf,
    lock_out: PathBuf,
    mods: usize,
    packages: usize,
    overrides: usize,
}

pub(crate) fn cmd_export(
    root: &Path,
    format: ExportArg,
    output: PathBuf,
    groups: Vec<String>,
    all_groups: bool,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<()> {
    let execution = execute_export(
        root, format, output, groups, all_groups, profiles, workspace,
    )?;
    if let Some(warning) = execution.compatibility_warning.as_ref() {
        eprintln!("{warning}");
    }
    if let Some(version) = execution.resolved_loader_version.as_ref() {
        println!("resolved loader version for export: {version}");
    }
    println!("exported {}", execution.file.display());
    Ok(())
}

pub(crate) fn build_export_report(
    root: &Path,
    format: ExportArg,
    output: PathBuf,
    groups: Vec<String>,
    all_groups: bool,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<CommandReport> {
    let execution = execute_export(
        root, format, output, groups, all_groups, profiles, workspace,
    )?;
    let mut lines = Vec::new();
    if let Some(warning) = execution.compatibility_warning.as_ref() {
        lines.push(warning.clone());
    }
    if let Some(version) = execution.resolved_loader_version.as_ref() {
        lines.push(format!("resolved loader version for export: {version}"));
    }
    lines.push(format!("exported {}", execution.file.display()));
    Ok(CommandReport {
        output: format!("{}\n", lines.join("\n")),
        exit_code: 0,
    })
}

pub(crate) fn build_export_json_report(
    root: &Path,
    format: ExportArg,
    output: PathBuf,
    groups: Vec<String>,
    all_groups: bool,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<ExportJsonReport> {
    let execution = execute_export(
        root, format, output, groups, all_groups, profiles, workspace,
    )?;
    let mut messages = Vec::new();
    if let Some(warning) = execution.compatibility_warning.as_ref() {
        messages.push(warning.clone());
    }
    if let Some(version) = execution.resolved_loader_version.as_ref() {
        messages.push(format!("resolved loader version for export: {version}"));
    }
    messages.push(format!("exported {}", execution.file.display()));
    Ok(ExportJsonReport {
        command: "export",
        groups: execution.groups,
        profiles: execution.profiles,
        format: execution.format,
        output: execution.file.display().to_string(),
        summary: ExportJsonSummary { exit_code: 0 },
        compatibility_warning: execution.compatibility_warning,
        resolved_loader_version: execution.resolved_loader_version,
        messages,
    })
}

fn execute_export(
    root: &Path,
    format: ExportArg,
    output: PathBuf,
    groups: Vec<String>,
    all_groups: bool,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<ExportExecution> {
    let manifest = load_manifest(root)?;
    let lock = load_lockfile_required(root)?;
    let active_groups =
        activation_groups_with_profiles(&manifest, workspace, &groups, all_groups, profiles)?;
    let profile_names = normalized_profile_names(profiles)?;
    ensure_lock_covers_groups(&manifest, &lock, &active_groups)?;
    let mut manifest = filtered_manifest_for_export(&manifest, &active_groups);
    let mut lock = filtered_lockfile(&lock, &active_groups);
    let compatibility_warning = matches!(format, ExportArg::Curseforge | ExportArg::Multimc)
        .then(|| {
            format!(
                "warning: `{}` export is compatibility-oriented and not part of the stable import/export baseline; validate it with your target launcher",
                format.as_str()
            )
        });
    let resolved_loader_version = resolve_loader_version(
        &manifest.project.minecraft,
        manifest.project.loader.kind,
        &manifest.project.loader.version,
    )
    .context(
        "failed to resolve project loader version for export (pin loader version to avoid network lookup)",
    )?;
    let resolved_loader_version_changed = !manifest
        .project
        .loader
        .version
        .eq_ignore_ascii_case(&resolved_loader_version);
    if resolved_loader_version_changed {
        manifest.project.loader.version = resolved_loader_version.clone();
        lock.metadata.loader.version = resolved_loader_version.clone();
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

    Ok(ExportExecution {
        compatibility_warning,
        resolved_loader_version: resolved_loader_version_changed.then_some(resolved_loader_version),
        file,
        groups: active_groups.into_iter().collect(),
        profiles: profile_names,
        format: format.as_str().to_string(),
    })
}

pub(crate) fn cmd_import(
    root: &Path,
    input: String,
    format: ImportFormatArg,
    side: ImportSideArg,
    force: bool,
) -> Result<()> {
    let execution = execute_import(root, input, format, side, force)?;
    print!("{}", render_import_execution(&execution));
    Ok(())
}

pub(crate) fn build_import_report(
    root: &Path,
    input: String,
    format: ImportFormatArg,
    side: ImportSideArg,
    force: bool,
) -> Result<CommandReport> {
    let execution = execute_import(root, input, format, side, force)?;
    Ok(CommandReport {
        output: render_import_execution(&execution),
        exit_code: 0,
    })
}

pub(crate) fn build_import_json_report(
    root: &Path,
    input: String,
    format: ImportFormatArg,
    side: ImportSideArg,
    force: bool,
) -> Result<ImportJsonReport> {
    let execution = execute_import(root, input, format, side, force)?;
    Ok(ImportJsonReport {
        command: "import",
        input: execution.input.clone(),
        detected_format: execution.detected.as_str().to_string(),
        side: execution.side,
        force: execution.force,
        summary: ImportJsonSummary {
            exit_code: 0,
            mods: execution.mods,
            packages: execution.packages,
            overrides: execution.overrides,
        },
        manifest_path: execution.manifest_out.display().to_string(),
        lockfile_path: execution.lock_out.display().to_string(),
        messages: vec![
            format!(
                "imported {} [{}]: mods={}, packages={}, overrides={}",
                execution.input,
                execution.detected.as_str(),
                execution.mods,
                execution.packages,
                execution.overrides
            ),
            format!("wrote {}", execution.manifest_out.display()),
            format!("wrote {}", execution.lock_out.display()),
        ],
    })
}

pub(crate) fn resolve_workspace_member_import_archive(
    input_root: &Path,
    member_path: &str,
) -> Result<PathBuf> {
    let member_relative = validate_workspace_member_import_path(member_path)?;
    let member_dir = input_root.join(member_relative);
    if !member_dir.is_dir() {
        bail!(
            "workspace import directory missing for member `{member_path}` at {}",
            member_dir.display()
        );
    }

    let mut candidates = Vec::new();
    for entry in fs::read_dir(&member_dir).with_context(|| {
        format!(
            "failed to read workspace import directory {}",
            member_dir.display()
        )
    })? {
        let entry = entry.with_context(|| {
            format!(
                "failed to inspect workspace import directory {}",
                member_dir.display()
            )
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if detect_pack_format(&path).is_ok() {
            candidates.push(path);
        }
    }
    candidates.sort();

    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        0 => bail!(
            "workspace import directory {} for member `{member_path}` must contain exactly one supported import archive",
            member_dir.display()
        ),
        _ => {
            let names = candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "workspace import directory {} for member `{member_path}` has multiple supported import archives: {names}",
                member_dir.display()
            )
        }
    }
}

pub(crate) fn build_workspace_member_import_report(
    root: &Path,
    input_root: &Path,
    member_path: &str,
    format: ImportFormatArg,
    side: ImportSideArg,
    force: bool,
) -> Result<CommandReport> {
    let archive = resolve_workspace_member_import_archive(input_root, member_path)?;
    build_import_report(root, archive.display().to_string(), format, side, force)
}

fn execute_import(
    root: &Path,
    input: String,
    format: ImportFormatArg,
    side: ImportSideArg,
    force: bool,
) -> Result<ImportExecution> {
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

    Ok(ImportExecution {
        input,
        detected,
        side: import_side_label(side).to_string(),
        force,
        manifest_out,
        lock_out,
        mods: imported.manifest.mods.len(),
        packages: imported.lockfile.packages.len(),
        overrides,
    })
}

fn render_import_execution(execution: &ImportExecution) -> String {
    format!(
        "imported {} [{}]: mods={}, packages={}, overrides={}\nwrote {}\nwrote {}\n",
        execution.input,
        execution.detected.as_str(),
        execution.mods,
        execution.packages,
        execution.overrides,
        execution.manifest_out.display(),
        execution.lock_out.display()
    )
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

fn import_side_label(side: ImportSideArg) -> &'static str {
    match side {
        ImportSideArg::Client => "client",
        ImportSideArg::Server => "server",
        ImportSideArg::Both => "both",
    }
}

fn validate_workspace_member_import_path(member_path: &str) -> Result<PathBuf> {
    let path = PathBuf::from(member_path);
    if path.as_os_str().is_empty() || path.is_absolute() {
        bail!("workspace member path `{member_path}` is invalid for workspace import");
    }
    if path
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        bail!("workspace member path `{member_path}` is invalid for workspace import");
    }
    Ok(path)
}

pub(crate) fn package_install_target_path(root: &Path, package: &LockedPackage) -> PathBuf {
    root.join(package.install_path_or_default())
}

pub(crate) fn workspace_member_export_output(
    base_output: &Path,
    member_name: &str,
    index: usize,
    total_members: usize,
) -> PathBuf {
    let width = total_members.max(1).to_string().len();
    let slug = workspace_member_export_slug(member_name);
    let suffix = format!("-{index:0width$}-{slug}");
    if let Some(extension) = base_output.extension() {
        let stem = base_output
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("modpack");
        let extension = extension.to_string_lossy();
        base_output.with_file_name(format!("{stem}{suffix}.{extension}"))
    } else {
        let file_name = base_output
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("modpack");
        base_output.with_file_name(format!("{file_name}{suffix}"))
    }
}

fn workspace_member_export_slug(member_name: &str) -> String {
    let mut slug = String::new();
    let mut previous_was_dash = false;
    for ch in member_name.chars() {
        let mapped = if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            ch
        } else {
            '-'
        };
        if mapped == '-' {
            if !previous_was_dash {
                slug.push('-');
            }
            previous_was_dash = true;
        } else {
            slug.push(mapped);
            previous_was_dash = false;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "member".to_string()
    } else {
        slug.to_string()
    }
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::cli::{ExportArg, ImportFormatArg, ImportSideArg};
    use crate::test_support::{TempProject, test_lockfile, test_manifest};
    use mineconda_core::{
        DEFAULT_GROUP_NAME, HashAlgorithm, LockedPackage, ModSide, ModSource, ModSpec, PackageHash,
        lockfile_path, manifest_path, write_lockfile, write_manifest,
    };
    use mineconda_export::{ExportFormat, ExportRequest, export_pack};
    use mineconda_resolver::{ResolveRequest, resolve_lockfile};
    use serde_json::Value;

    #[test]
    fn workspace_member_export_output_suffixes_member_without_extension() {
        let output =
            workspace_member_export_output(Path::new("/tmp/dist/modpack"), "packs/client", 2, 12);
        assert_eq!(output, Path::new("/tmp/dist/modpack-02-packs-client"));
    }

    #[test]
    fn workspace_member_export_output_suffixes_member_before_extension() {
        let output = workspace_member_export_output(
            Path::new("/tmp/dist/modpack.mrpack"),
            "packs/client",
            1,
            12,
        );
        assert_eq!(
            output,
            Path::new("/tmp/dist/modpack-01-packs-client.mrpack")
        );
    }

    #[test]
    fn workspace_member_export_output_normalizes_empty_member_slug() {
        let output = workspace_member_export_output(Path::new("dist/bundle"), "///", 3, 12);
        assert_eq!(output, Path::new("dist/bundle-03-member"));
    }

    #[test]
    fn build_export_json_report_serializes_output_metadata() {
        let project = TempProject::new("export-json-report");
        fs::create_dir_all(project.path.join("vendor")).expect("vendor dir");
        fs::write(project.path.join("vendor/demo.jar"), b"demo").expect("demo jar");
        let mut manifest = test_manifest(vec![ModSpec::new(
            "demo".to_string(),
            ModSource::Local,
            "vendor/demo.jar".to_string(),
            ModSide::Both,
        )]);
        manifest.project.loader.version = "21.1.227".to_string();
        write_manifest(&manifest_path(&project.path), &manifest).expect("write manifest");
        let output = resolve_lockfile(
            &manifest,
            None,
            &ResolveRequest {
                upgrade: false,
                groups: std::collections::BTreeSet::from([DEFAULT_GROUP_NAME.to_string()]),
            },
        )
        .expect("resolve lock");
        write_lockfile(&lockfile_path(&project.path), &output.lockfile).expect("write lock");

        let report = build_export_json_report(
            &project.path,
            ExportArg::ModsDesc,
            PathBuf::from("dist/modpack"),
            Vec::new(),
            false,
            &[],
            None,
        )
        .expect("export json report");

        let value: Value = serde_json::to_value(&report).expect("serialize report");
        assert_eq!(value["command"], "export");
        assert_eq!(value["format"], "mods-desc");
        assert_eq!(value["summary"]["exit_code"], 0);
        assert!(
            value["output"]
                .as_str()
                .unwrap_or("")
                .ends_with("dist/modpack.json")
        );
    }

    #[test]
    fn resolve_workspace_member_import_archive_rejects_missing_member_dir() {
        let input_root = TempProject::new("workspace-import-missing");
        let err = resolve_workspace_member_import_archive(&input_root.path, "packs/client")
            .expect_err("missing member dir should fail");
        assert!(
            format!("{err:#}")
                .contains("workspace import directory missing for member `packs/client`"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn resolve_workspace_member_import_archive_rejects_zero_supported_candidates() {
        let input_root = TempProject::new("workspace-import-zero");
        let member_dir = input_root.path.join("packs/client");
        fs::create_dir_all(&member_dir).expect("member dir");
        fs::write(member_dir.join("README.txt"), b"not an archive").expect("readme");

        let err = resolve_workspace_member_import_archive(&input_root.path, "packs/client")
            .expect_err("missing archive should fail");
        assert!(
            format!("{err:#}").contains("must contain exactly one supported import archive"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn resolve_workspace_member_import_archive_rejects_multiple_supported_candidates() {
        let input_root = TempProject::new("workspace-import-multiple");
        let member_dir = input_root.path.join("packs/client");
        fs::create_dir_all(&member_dir).expect("member dir");
        write_test_mrpack(&member_dir, "client-a.mrpack");
        write_test_mrpack(&member_dir, "client-b.mrpack");

        let err = resolve_workspace_member_import_archive(&input_root.path, "packs/client")
            .expect_err("multiple archives should fail");
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("multiple supported import archives"),
            "unexpected error: {rendered}"
        );
        assert!(
            rendered.contains("client-a.mrpack"),
            "unexpected error: {rendered}"
        );
        assert!(
            rendered.contains("client-b.mrpack"),
            "unexpected error: {rendered}"
        );
    }

    #[test]
    fn build_workspace_member_import_report_imports_selected_archive() {
        let input_root = TempProject::new("workspace-import-success-input");
        let member_dir = input_root.path.join("packs/client");
        fs::create_dir_all(&member_dir).expect("member dir");
        let archive = write_test_mrpack(&member_dir, "client-pack.mrpack");

        let project = TempProject::new("workspace-import-success-project");
        let report = build_workspace_member_import_report(
            &project.path,
            &input_root.path,
            "packs/client",
            ImportFormatArg::Auto,
            ImportSideArg::Client,
            false,
        )
        .expect("workspace member import should succeed");

        assert!(
            report
                .output
                .contains(&format!("imported {} [modrinth-mrpack]", archive.display())),
            "unexpected report: {}",
            report.output
        );
        assert!(
            manifest_path(&project.path).exists(),
            "manifest should be written"
        );
        assert!(
            lockfile_path(&project.path).exists(),
            "lockfile should be written"
        );
    }

    #[test]
    fn build_import_json_report_serializes_import_result() {
        let fixture_root = TempProject::new("import-json-fixture-root");
        let archive = write_test_mrpack(&fixture_root.path, "client-pack.mrpack");
        let project = TempProject::new("import-json-report");

        let report = build_import_json_report(
            &project.path,
            archive.display().to_string(),
            ImportFormatArg::Auto,
            ImportSideArg::Client,
            false,
        )
        .expect("import json report");

        let value: Value = serde_json::to_value(&report).expect("serialize report");
        assert_eq!(value["command"], "import");
        assert_eq!(value["detected_format"], "modrinth-mrpack");
        assert_eq!(value["side"], "client");
        assert_eq!(value["summary"]["mods"], 1);
        assert_eq!(value["summary"]["packages"], 1);
        assert_eq!(value["summary"]["exit_code"], 0);
        assert!(
            manifest_path(&project.path).exists(),
            "manifest should be written"
        );
        assert!(
            lockfile_path(&project.path).exists(),
            "lockfile should be written"
        );
    }

    fn write_test_mrpack(dir: &Path, file_name: &str) -> PathBuf {
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
}
