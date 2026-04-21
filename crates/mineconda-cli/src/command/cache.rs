use crate::*;
pub(crate) fn cmd_cache(root: &Path, command: CacheCommands) -> Result<()> {
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
