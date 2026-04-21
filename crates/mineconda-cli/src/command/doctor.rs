use crate::*;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoctorLevel {
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
pub(crate) struct DoctorFinding {
    pub(crate) level: DoctorLevel,
    pub(crate) title: &'static str,
    pub(crate) detail: String,
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

pub(crate) fn collect_s3_doctor_findings<F>(
    manifest: &Manifest,
    mut has_env: F,
) -> Vec<DoctorFinding>
where
    F: FnMut(&str) -> bool,
{
    let has_s3_source_mods = manifest
        .mods
        .iter()
        .any(|entry| entry.source == ModSource::S3);
    let has_any_s3 =
        has_s3_source_mods || manifest.sources.s3.is_some() || manifest.cache.s3.is_some();
    let Some(s3_cache) = manifest.cache.s3.as_ref() else {
        if !has_any_s3 {
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
            manifest.sources.s3.is_some(),
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
        manifest.sources.s3.is_some(),
        has_s3_source_mods,
    ));
    findings.extend(collect_s3_cache_doctor_findings(s3_cache, &mut has_env));
    findings
}

fn collect_s3_source_doctor_findings(
    source: Option<&S3SourceConfig>,
    has_source_config: bool,
    has_s3_source_mods: bool,
) -> Vec<DoctorFinding> {
    if !has_s3_source_mods && !has_source_config {
        return Vec::new();
    }

    let finding = match source {
        Some(s3) if !s3.bucket.trim().is_empty() => {
            let mut detail = format!(
                "bucket={} delivery={}",
                s3.bucket,
                s3_delivery_mode(
                    s3.public_base_url.as_deref(),
                    s3.endpoint.as_deref(),
                    s3.path_style,
                )
            );
            if let Some(prefix) = s3
                .key_prefix
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                detail.push_str(&format!(" prefix={prefix}"));
            }
            if !has_s3_source_mods {
                detail.push_str(" unused-by-current-mods");
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
        let mut detail = format!(
            "enabled bucket={} delivery={} upload={}",
            s3_cache.bucket,
            s3_delivery_mode(
                s3_cache.public_base_url.as_deref(),
                s3_cache.endpoint.as_deref(),
                s3_cache.path_style,
            ),
            if s3_cache.upload_enabled {
                "enabled"
            } else {
                "disabled"
            }
        );
        if let Some(prefix) = s3_cache
            .prefix
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            detail.push_str(&format!(" prefix={prefix}"));
        }
        findings.push(DoctorFinding::new(
            DoctorLevel::Ok,
            "s3 cache config",
            detail,
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

pub(crate) fn collect_s3_remote_smoke_doctor_findings<F>(
    smoke_enabled: bool,
    mut has_env: F,
) -> Vec<DoctorFinding>
where
    F: FnMut(&str) -> bool,
{
    if !smoke_enabled {
        return Vec::new();
    }

    let mut findings = vec![DoctorFinding::new(
        DoctorLevel::Ok,
        "s3 smoke status",
        "experimental remote smoke requested by MINECONDA_ENABLE_S3_SMOKE=1",
    )];

    findings.push(if has_env("MINECONDA_S3_SSH_TARGET") {
        DoctorFinding::new(
            DoctorLevel::Ok,
            "s3 smoke target",
            "remote smoke target configured",
        )
    } else {
        DoctorFinding::new(
            DoctorLevel::Warn,
            "s3 smoke target",
            "MINECONDA_S3_SSH_TARGET is not set",
        )
    });

    findings.push(if has_env("MINECONDA_S3_REMOTE_PRIVILEGE_SECRET") {
        DoctorFinding::new(
            DoctorLevel::Ok,
            "s3 smoke privilege",
            "remote privilege secret is set",
        )
    } else {
        DoctorFinding::new(
            DoctorLevel::Warn,
            "s3 smoke privilege",
            "MINECONDA_S3_REMOTE_PRIVILEGE_SECRET is not set; remote automation must rely on passwordless container access or another non-interactive privilege path",
        )
    });

    findings
}

fn s3_delivery_mode(
    public_base_url: Option<&str>,
    endpoint: Option<&str>,
    path_style: bool,
) -> &'static str {
    if public_base_url.is_some_and(|value| !value.trim().is_empty()) {
        "public-base-url"
    } else if endpoint.is_some_and(|value| !value.trim().is_empty()) {
        if path_style {
            "custom-endpoint(path-style)"
        } else {
            "custom-endpoint(virtual-hosted)"
        }
    } else {
        "derived-service-url"
    }
}

fn experimental_s3_smoke_enabled() -> bool {
    env::var("MINECONDA_ENABLE_S3_SMOKE")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim(),
                "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
            )
        })
}

pub(crate) fn cmd_doctor(root: &Path, strict: bool, no_color: bool) -> Result<()> {
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
        let has_any_s3 = manifest
            .mods
            .iter()
            .any(|entry| entry.source == ModSource::S3)
            || manifest.sources.s3.is_some()
            || manifest.cache.s3.is_some();
        if has_any_s3 || experimental_s3_smoke_enabled() {
            for finding in
                collect_s3_remote_smoke_doctor_findings(experimental_s3_smoke_enabled(), |name| {
                    env::var_os(name).is_some()
                })
            {
                doctor_log(
                    &mut counts,
                    finding.level,
                    finding.title,
                    finding.detail,
                    use_color,
                );
            }
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

#[cfg(test)]
mod tests {
    use mineconda_core::Manifest;

    use super::*;

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
        assert!(findings.iter().any(|finding| {
            finding.title == "s3 cache config"
                && finding.detail.contains("delivery=derived-service-url")
        }));
    }

    #[test]
    fn s3_doctor_findings_surface_remote_smoke_env_without_values() {
        let findings = collect_s3_remote_smoke_doctor_findings(true, |name| {
            matches!(name, "MINECONDA_S3_SSH_TARGET")
        });

        assert!(findings.iter().any(|finding| {
            finding.title == "s3 smoke status"
                && finding.detail.contains("MINECONDA_ENABLE_S3_SMOKE=1")
        }));
        assert!(findings.iter().any(|finding| {
            finding.title == "s3 smoke target" && finding.level == DoctorLevel::Ok
        }));
        assert!(findings.iter().any(|finding| {
            finding.title == "s3 smoke privilege"
                && finding.level == DoctorLevel::Warn
                && finding
                    .detail
                    .contains("MINECONDA_S3_REMOTE_PRIVILEGE_SECRET")
        }));
        assert!(findings.iter().all(|finding| {
            !finding.detail.contains("ssh://") && !finding.detail.contains("127.0.0.1")
        }));
    }
}
