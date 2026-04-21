use crate::*;
pub(crate) fn cmd_export(
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
