use crate::*;

#[derive(Debug, Clone)]
struct ExportExecution {
    compatibility_warning: Option<String>,
    resolved_loader_version: Option<String>,
    file: PathBuf,
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
    })
}

pub(crate) fn cmd_import(
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
}
