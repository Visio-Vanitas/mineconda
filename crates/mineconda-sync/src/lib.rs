use std::collections::HashSet;
use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hmac::{Hmac, Mac};
use mineconda_core::{
    HashAlgorithm, LockMetadata, LockedPackage, Lockfile, ModSource, S3CacheAuth, S3CacheConfig,
    http_user_agent,
};
use quick_xml::de::from_str as from_xml_str;
use rayon::ThreadPoolBuilder;
use rayon::prelude::*;
use reqwest::blocking::{Client, Response};
use reqwest::{Method, StatusCode, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone)]
pub struct SyncRequest {
    pub project_root: PathBuf,
    pub prune: bool,
    pub s3_cache: Option<S3CacheConfig>,
    pub offline: bool,
    pub jobs: usize,
    pub verbose_cache: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncReport {
    pub package_count: usize,
    pub local_hits: usize,
    pub s3_hits: usize,
    pub origin_downloads: usize,
    pub installed: usize,
    pub removed: usize,
    pub failed: usize,
    pub lockfile_updated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheHitSource {
    Local,
    S3,
    Origin,
}

impl CacheHitSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::S3 => "s3",
            Self::Origin => "origin",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CacheStatsReport {
    pub file_count: usize,
    pub total_bytes: u64,
    pub referenced_files: usize,
    pub referenced_bytes: u64,
    pub unreferenced_files: usize,
    pub unreferenced_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CacheVerifyReport {
    pub checked: usize,
    pub valid: usize,
    pub invalid: usize,
    pub missing: usize,
    pub repaired: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone)]
pub struct RemotePruneRequest {
    pub max_age_days: u64,
    pub prefix: Option<String>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RemotePruneReport {
    pub listed: usize,
    pub candidates: usize,
    pub deleted: usize,
    pub retained: usize,
}

pub fn sync_lockfile(lockfile: &mut Lockfile, request: &SyncRequest) -> Result<SyncReport> {
    let client = Client::builder()
        .user_agent(http_user_agent())
        .build()
        .context("failed to build HTTP client for sync")?;

    let cache_root = cache_root()?;
    fs::create_dir_all(&cache_root)
        .with_context(|| format!("failed to create cache dir {}", cache_root.display()))?;

    let mods_dir = request.project_root.join("mods");
    fs::create_dir_all(&mods_dir)
        .with_context(|| format!("failed to create mods dir {}", mods_dir.display()))?;

    let s3_cache = configure_s3_cache_backend(request.s3_cache.as_ref())?;
    let jobs = request.jobs.max(1);
    let metadata = lockfile.metadata.clone();
    let project_root = request.project_root.clone();
    let cache_root_for_tasks = cache_root.clone();
    let client_for_tasks = client.clone();
    let s3_cache_for_tasks = s3_cache.clone();

    let pool = ThreadPoolBuilder::new()
        .num_threads(jobs)
        .build()
        .context("failed to build sync worker pool")?;

    let outcomes = pool.install(|| {
        lockfile
            .packages
            .par_iter()
            .map(|package| {
                ensure_cached_package(
                    package,
                    &cache_root_for_tasks,
                    &project_root,
                    &metadata,
                    s3_cache_for_tasks.as_ref(),
                    &client_for_tasks,
                    request.offline,
                )
            })
            .collect::<Vec<_>>()
    });

    let mut resolved_outcomes = Vec::with_capacity(outcomes.len());
    for outcome in outcomes {
        resolved_outcomes.push(outcome?);
    }

    let mut report = SyncReport {
        package_count: lockfile.packages.len(),
        ..SyncReport::default()
    };
    let mut expected_mod_files = HashSet::new();

    for (package, outcome) in lockfile
        .packages
        .iter_mut()
        .zip(resolved_outcomes.into_iter())
    {
        match outcome.source {
            CacheHitSource::Local => report.local_hits += 1,
            CacheHitSource::S3 => report.s3_hits += 1,
            CacheHitSource::Origin => report.origin_downloads += 1,
        }

        if request.verbose_cache {
            println!(
                "cache {} [{}] -> {}",
                package.id,
                package.source.as_str(),
                outcome.source.as_str()
            );
        }

        let metadata_changed = update_package_metadata(package, &outcome.cache_path)?;
        report.lockfile_updated = report.lockfile_updated || metadata_changed;
        let cache_path = canonicalize_cache_path(&cache_root, package, &outcome.cache_path)?;

        let relative_target = package_install_relative_path(package)?;
        let target = request.project_root.join(&relative_target);
        install_package_file(&cache_path, &target)?;
        if let Some(mod_file_name) = mods_root_file_name(&relative_target) {
            expected_mod_files.insert(mod_file_name);
        }
        report.installed += 1;
    }

    if request.prune {
        report.removed += prune_mods_directory(&mods_dir, &expected_mod_files)?;
    }

    Ok(report)
}

pub fn collect_cache_stats(lock: Option<&Lockfile>, cache_root: &Path) -> Result<CacheStatsReport> {
    let referenced_paths = expected_cache_paths(lock, cache_root);
    let mut report = CacheStatsReport::default();

    if !cache_root.exists() {
        return Ok(report);
    }

    for entry in fs::read_dir(cache_root)
        .with_context(|| format!("failed to read {}", cache_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let size = entry.metadata()?.len();
        report.file_count += 1;
        report.total_bytes += size;
        if referenced_paths.contains(&path) {
            report.referenced_files += 1;
            report.referenced_bytes += size;
        } else {
            report.unreferenced_files += 1;
            report.unreferenced_bytes += size;
        }
    }

    Ok(report)
}

pub fn verify_cache_entries(
    lock: Option<&Lockfile>,
    cache_root: &Path,
    repair: bool,
) -> Result<CacheVerifyReport> {
    let mut report = CacheVerifyReport::default();
    if !cache_root.exists() {
        return Ok(report);
    }

    let Some(lock) = lock else {
        for entry in fs::read_dir(cache_root)
            .with_context(|| format!("failed to read {}", cache_root.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            report.checked += 1;
            match compute_sha256_and_size(&entry.path()) {
                Ok(_) => report.valid += 1,
                Err(_) => {
                    report.invalid += 1;
                    if repair {
                        fs::remove_file(entry.path()).with_context(|| {
                            format!("failed to remove {}", entry.path().display())
                        })?;
                        report.repaired += 1;
                    }
                }
            }
        }
        return Ok(report);
    };

    let referenced_paths = expected_cache_paths(Some(lock), cache_root);
    for package in &lock.packages {
        let cache_path = cache_path_for_package(cache_root, package);
        if !cache_path.exists() {
            report.missing += 1;
            continue;
        }

        report.checked += 1;
        if verify_existing_cache(package, &cache_path)? {
            report.valid += 1;
        } else {
            report.invalid += 1;
            if repair {
                fs::remove_file(&cache_path)
                    .with_context(|| format!("failed to remove {}", cache_path.display()))?;
                report.repaired += 1;
            }
        }
    }

    for entry in fs::read_dir(cache_root)
        .with_context(|| format!("failed to read {}", cache_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        if !referenced_paths.contains(&entry.path()) {
            report.skipped += 1;
        }
    }

    Ok(report)
}

pub fn remote_prune_s3_cache(
    config: &S3CacheConfig,
    request: &RemotePruneRequest,
) -> Result<RemotePruneReport> {
    let client = Client::builder()
        .user_agent(http_user_agent())
        .build()
        .context("failed to build HTTP client for remote prune")?;
    let backend = configure_s3_cache_backend(Some(config))?
        .context("cache.s3 must be configured and enabled for remote prune")?;

    let mut report = RemotePruneReport::default();
    let effective_prefix = request
        .prefix
        .clone()
        .or_else(|| backend.config.prefix.clone());
    let cutoff = OffsetDateTime::now_utc()
        - time::Duration::days(i64::try_from(request.max_age_days).unwrap_or(i64::MAX));
    let mut continuation_token = None::<String>;
    let mut candidates = Vec::new();

    loop {
        let target = build_s3_list_target(
            &backend.config,
            effective_prefix.as_deref(),
            continuation_token.as_deref(),
        )?;
        let response = execute_s3_request(&client, Method::GET, &target, None, &backend, None)?;
        let response = response
            .error_for_status()
            .with_context(|| format!("s3 list returned error for {}", target.url))?;
        let body = response
            .text()
            .with_context(|| format!("failed to decode {}", target.url))?;
        let listing: ListBucketResult =
            from_xml_str(&body).context("failed to parse s3 list objects response")?;

        report.listed += listing.contents.len();
        let (page_candidates, page_retained) =
            partition_prune_candidates(listing.contents, cutoff)?;
        candidates.extend(page_candidates);
        report.retained += page_retained;

        if listing.is_truncated {
            continuation_token = listing.next_continuation_token;
            if continuation_token.is_none() {
                break;
            }
        } else {
            break;
        }
    }

    report.candidates = candidates.len();
    if request.dry_run {
        report.retained += report.candidates;
        return Ok(report);
    }

    for item in candidates {
        let target =
            build_s3_object_target(&backend.config, &item.key, backend.uses_signed_auth())?;
        let response = execute_s3_request(&client, Method::DELETE, &target, None, &backend, None)?;
        response
            .error_for_status()
            .with_context(|| format!("s3 delete returned error for {}", target.url))?;
        report.deleted += 1;
    }

    Ok(report)
}

fn partition_prune_candidates(
    items: Vec<ListBucketItem>,
    cutoff: OffsetDateTime,
) -> Result<(Vec<ListBucketItem>, usize)> {
    let mut candidates = Vec::new();
    let mut retained = 0usize;

    for item in items {
        let last_modified = OffsetDateTime::parse(item.last_modified.trim(), &Rfc3339)
            .with_context(|| format!("invalid s3 LastModified `{}`", item.last_modified))?;
        if last_modified < cutoff {
            candidates.push(item);
        } else {
            retained += 1;
        }
    }

    Ok((candidates, retained))
}

struct CacheOutcome {
    cache_path: PathBuf,
    source: CacheHitSource,
}

#[derive(Debug, Clone)]
struct S3CacheBackend {
    config: S3CacheConfig,
    auth: ResolvedS3Auth,
}

impl S3CacheBackend {
    fn uses_signed_auth(&self) -> bool {
        matches!(self.auth, ResolvedS3Auth::SigV4(_))
    }
}

#[derive(Debug, Clone)]
enum ResolvedS3Auth {
    Anonymous,
    SigV4(S3Credentials),
}

#[derive(Debug, Clone)]
struct S3Credentials {
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
    region: String,
}

#[derive(Debug, Clone)]
struct S3Target {
    url: String,
    host_header: String,
    canonical_uri: String,
    query_pairs: Vec<(String, String)>,
}

#[derive(Debug, Deserialize, Default)]
struct ListBucketResult {
    #[serde(rename = "Contents", default)]
    contents: Vec<ListBucketItem>,
    #[serde(rename = "IsTruncated", default)]
    is_truncated: bool,
    #[serde(rename = "NextContinuationToken", default)]
    next_continuation_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListBucketItem {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "LastModified")]
    last_modified: String,
    #[serde(rename = "Size")]
    _size: u64,
}

fn configure_s3_cache_backend(config: Option<&S3CacheConfig>) -> Result<Option<S3CacheBackend>> {
    let Some(config) = config else {
        return Ok(None);
    };
    if !config.enabled {
        return Ok(None);
    }
    if config.bucket.trim().is_empty() {
        eprintln!("warning: s3 cache disabled because cache.s3.bucket is empty");
        return Ok(None);
    }

    Ok(Some(S3CacheBackend {
        config: config.clone(),
        auth: resolve_s3_auth(config)?,
    }))
}

fn resolve_s3_auth(config: &S3CacheConfig) -> Result<ResolvedS3Auth> {
    match config.auth {
        S3CacheAuth::Anonymous => Ok(ResolvedS3Auth::Anonymous),
        S3CacheAuth::Auto => Ok(resolve_s3_credentials(config)?
            .map(ResolvedS3Auth::SigV4)
            .unwrap_or(ResolvedS3Auth::Anonymous)),
        S3CacheAuth::Sigv4 => Ok(ResolvedS3Auth::SigV4(resolve_s3_credentials_required(
            config,
        )?)),
    }
}

fn resolve_s3_credentials(config: &S3CacheConfig) -> Result<Option<S3Credentials>> {
    let Some(access_key_env) = configured_env_name(config.access_key_env.as_deref()) else {
        return Ok(None);
    };
    let Some(secret_key_env) = configured_env_name(config.secret_key_env.as_deref()) else {
        return Ok(None);
    };

    let Some(access_key) = env::var(access_key_env)
        .ok()
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let Some(secret_key) = env::var(secret_key_env)
        .ok()
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };

    let session_token = configured_env_name(config.session_token_env.as_deref())
        .and_then(|name| env::var(name).ok())
        .filter(|value| !value.is_empty());
    let region = config
        .region
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("us-east-1")
        .to_string();
    Ok(Some(S3Credentials {
        access_key,
        secret_key,
        session_token,
        region,
    }))
}

fn resolve_s3_credentials_required(config: &S3CacheConfig) -> Result<S3Credentials> {
    let access_key_env = configured_env_name(config.access_key_env.as_deref())
        .context("cache.s3.auth=sigv4 requires cache.s3.access_key_env")?;
    let secret_key_env = configured_env_name(config.secret_key_env.as_deref())
        .context("cache.s3.auth=sigv4 requires cache.s3.secret_key_env")?;
    let access_key = env::var(access_key_env)
        .ok()
        .filter(|value| !value.is_empty())
        .with_context(|| format!("environment variable `{access_key_env}` is not set"))?;
    let secret_key = env::var(secret_key_env)
        .ok()
        .filter(|value| !value.is_empty())
        .with_context(|| format!("environment variable `{secret_key_env}` is not set"))?;
    let session_token = configured_env_name(config.session_token_env.as_deref())
        .and_then(|name| env::var(name).ok())
        .filter(|value| !value.is_empty());
    let region = config
        .region
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("us-east-1")
        .to_string();
    Ok(S3Credentials {
        access_key,
        secret_key,
        session_token,
        region,
    })
}

fn configured_env_name(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn ensure_cached_package(
    package: &LockedPackage,
    cache_root: &Path,
    project_root: &Path,
    metadata: &LockMetadata,
    s3_cache: Option<&S3CacheBackend>,
    client: &Client,
    offline: bool,
) -> Result<CacheOutcome> {
    let cache_path = cache_path_for_package(cache_root, package);

    if cache_path.exists() {
        if verify_existing_cache(package, &cache_path)? {
            return Ok(CacheOutcome {
                cache_path,
                source: CacheHitSource::Local,
            });
        }
        fs::remove_file(&cache_path).with_context(|| {
            format!("failed to delete stale cache file {}", cache_path.display())
        })?;
    }

    if offline {
        bail!(
            "offline sync cannot fetch missing cache for {} [{}]",
            package.id,
            package.source.as_str()
        );
    }

    let tmp_path = cache_path.with_extension("part");
    if tmp_path.exists() {
        fs::remove_file(&tmp_path)
            .with_context(|| format!("failed to delete temp file {}", tmp_path.display()))?;
    }

    let source = if let Some(s3_cache) = s3_cache {
        match fetch_from_s3_cache(package, metadata, s3_cache, client, &tmp_path) {
            Ok(true) => CacheHitSource::S3,
            Ok(false) => {
                fetch_package_to_path(package, project_root, client, &tmp_path)?;
                CacheHitSource::Origin
            }
            Err(err) => {
                eprintln!("warning: s3 cache fetch failed for {}: {err:#}", package.id);
                fetch_package_to_path(package, project_root, client, &tmp_path)?;
                CacheHitSource::Origin
            }
        }
    } else {
        fetch_package_to_path(package, project_root, client, &tmp_path)?;
        CacheHitSource::Origin
    };

    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create cache parent {}", parent.display()))?;
    }

    if let Err(rename_error) = fs::rename(&tmp_path, &cache_path) {
        fs::copy(&tmp_path, &cache_path)
            .with_context(|| format!("failed to copy {} to cache", tmp_path.display()))?;
        fs::remove_file(&tmp_path)
            .with_context(|| format!("failed to remove temp file {}", tmp_path.display()))?;
        if !cache_path.exists() {
            bail!(
                "failed to move {} into cache: {}",
                tmp_path.display(),
                rename_error
            );
        }
    }

    if !verify_existing_cache(package, &cache_path)? {
        bail!(
            "cached package failed post-write verification: {}",
            cache_path.display()
        );
    }

    if matches!(source, CacheHitSource::Origin)
        && let Some(s3_cache) = s3_cache
        && s3_cache.config.upload_enabled
        && let Err(err) = upload_to_s3_cache(package, metadata, s3_cache, client, &cache_path)
    {
        eprintln!(
            "warning: s3 cache upload failed for {}: {err:#}",
            package.id
        );
    }

    Ok(CacheOutcome { cache_path, source })
}

fn fetch_from_s3_cache(
    package: &LockedPackage,
    metadata: &LockMetadata,
    s3_cache: &S3CacheBackend,
    client: &Client,
    destination: &Path,
) -> Result<bool> {
    let key = s3_cache_object_key(&s3_cache.config, metadata, package);
    let target = build_s3_object_target(&s3_cache.config, &key, s3_cache.uses_signed_auth())?;
    let response = execute_s3_request(client, Method::GET, &target, None, s3_cache, None)
        .with_context(|| format!("failed to query s3 cache object {}", target.url))?;

    if response.status() == StatusCode::NOT_FOUND {
        return Ok(false);
    }
    if !response.status().is_success() {
        bail!(
            "s3 cache fetch returned {} for {}",
            response.status(),
            target.url
        );
    }

    write_response_to_path(response, destination)?;
    Ok(true)
}

fn upload_to_s3_cache(
    package: &LockedPackage,
    metadata: &LockMetadata,
    s3_cache: &S3CacheBackend,
    client: &Client,
    source_path: &Path,
) -> Result<()> {
    let key = s3_cache_object_key(&s3_cache.config, metadata, package);
    let target = build_s3_object_target(&s3_cache.config, &key, s3_cache.uses_signed_auth())?;
    let body = fs::read(source_path)
        .with_context(|| format!("failed to read {}", source_path.display()))?;
    let response = execute_s3_request(
        client,
        Method::PUT,
        &target,
        Some(body),
        s3_cache,
        Some("application/java-archive"),
    )
    .with_context(|| format!("failed to upload s3 cache object {}", target.url))?;

    if !response.status().is_success() {
        bail!(
            "s3 cache upload returned {} for {}",
            response.status(),
            target.url
        );
    }
    Ok(())
}

fn execute_s3_request(
    client: &Client,
    method: Method,
    target: &S3Target,
    body: Option<Vec<u8>>,
    backend: &S3CacheBackend,
    content_type: Option<&str>,
) -> Result<Response> {
    let mut builder = client.request(method.clone(), &target.url);
    if let Some(content_type) = content_type {
        builder = builder.header(reqwest::header::CONTENT_TYPE, content_type);
    }

    match &backend.auth {
        ResolvedS3Auth::Anonymous => {
            if let Some(body) = body {
                builder = builder.body(body);
            }
            builder
                .send()
                .with_context(|| format!("failed to send {}", target.url))
        }
        ResolvedS3Auth::SigV4(credentials) => {
            let body = body.unwrap_or_default();
            let signed = sign_s3_request(
                &method,
                target,
                &body,
                credentials,
                OffsetDateTime::now_utc(),
            )?;
            builder = builder
                .header(reqwest::header::HOST, signed.host_header)
                .header("x-amz-date", signed.amz_date)
                .header("x-amz-content-sha256", signed.payload_sha256)
                .header(reqwest::header::AUTHORIZATION, signed.authorization);
            if let Some(session_token) = signed.session_token {
                builder = builder.header("x-amz-security-token", session_token);
            }
            if !body.is_empty() {
                builder = builder.body(body);
            }
            builder
                .send()
                .with_context(|| format!("failed to send signed request {}", target.url))
        }
    }
}

struct SignedHeaders {
    host_header: String,
    amz_date: String,
    payload_sha256: String,
    authorization: String,
    session_token: Option<String>,
}

fn sign_s3_request(
    method: &Method,
    target: &S3Target,
    body: &[u8],
    credentials: &S3Credentials,
    now: OffsetDateTime,
) -> Result<SignedHeaders> {
    let amz_date = format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
    let date_stamp = format!(
        "{:04}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    );
    let payload_sha256 = hex_sha256(body);

    let mut canonical_headers = vec![
        ("host".to_string(), target.host_header.clone()),
        ("x-amz-content-sha256".to_string(), payload_sha256.clone()),
        ("x-amz-date".to_string(), amz_date.clone()),
    ];
    if let Some(session_token) = credentials.session_token.as_ref() {
        canonical_headers.push(("x-amz-security-token".to_string(), session_token.clone()));
    }
    canonical_headers.sort_by(|a, b| a.0.cmp(&b.0));

    let canonical_headers_str = canonical_headers
        .iter()
        .map(|(name, value)| format!("{}:{}\n", name, value.trim()))
        .collect::<String>();
    let signed_headers = canonical_headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_query = canonical_query_string(&target.query_pairs);
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method.as_str(),
        target.canonical_uri,
        canonical_query,
        canonical_headers_str,
        signed_headers,
        payload_sha256
    );

    let credential_scope = format!("{}/{}/s3/aws4_request", date_stamp, credentials.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        amz_date,
        credential_scope,
        hex_sha256(canonical_request.as_bytes())
    );
    let signing_key =
        derive_signing_key(&credentials.secret_key, &date_stamp, &credentials.region)?;
    let signature = hex_hmac_sha256(&signing_key, string_to_sign.as_bytes())?;
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        credentials.access_key, credential_scope, signed_headers, signature
    );

    Ok(SignedHeaders {
        host_header: target.host_header.clone(),
        amz_date,
        payload_sha256,
        authorization,
        session_token: credentials.session_token.clone(),
    })
}

fn derive_signing_key(secret_key: &str, date_stamp: &str, region: &str) -> Result<Vec<u8>> {
    let k_date = hmac_sha256(
        format!("AWS4{secret_key}").as_bytes(),
        date_stamp.as_bytes(),
    )?;
    let k_region = hmac_sha256(&k_date, region.as_bytes())?;
    let k_service = hmac_sha256(&k_region, b"s3")?;
    hmac_sha256(&k_service, b"aws4_request")
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let mut mac = HmacSha256::new_from_slice(key).context("failed to create HMAC state")?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn hex_hmac_sha256(key: &[u8], data: &[u8]) -> Result<String> {
    Ok(hex::encode(hmac_sha256(key, data)?))
}

fn hex_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn canonical_query_string(pairs: &[(String, String)]) -> String {
    let mut encoded = pairs
        .iter()
        .map(|(key, value)| {
            (
                encode_uri_component(key, false),
                encode_uri_component(value, false),
            )
        })
        .collect::<Vec<_>>();
    encoded.sort();
    encoded
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn build_s3_object_target(
    config: &S3CacheConfig,
    key: &str,
    prefer_signed_url: bool,
) -> Result<S3Target> {
    let bucket = config.bucket.trim();
    if bucket.is_empty() {
        bail!("cache.s3.bucket cannot be empty");
    }

    let encoded_key = encode_uri_component(key, true);
    if !prefer_signed_url
        && let Some(base) = config.public_base_url.as_deref()
        && !base.trim().is_empty()
    {
        let url = format!("{}/{}", base.trim().trim_end_matches('/'), encoded_key);
        return target_from_url(url, format!("/{encoded_key}"), Vec::new());
    }

    if let Some(endpoint) = config.endpoint.as_deref()
        && !endpoint.trim().is_empty()
    {
        let endpoint = endpoint.trim().trim_end_matches('/');
        if config.path_style {
            let url = format!("{endpoint}/{bucket}/{encoded_key}");
            return target_from_url(url, format!("/{bucket}/{encoded_key}"), Vec::new());
        }
        let url = s3_virtual_host_url(endpoint, bucket, &encoded_key);
        return target_from_url(url, format!("/{encoded_key}"), Vec::new());
    }

    let region = config
        .region
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("us-east-1");
    if config.path_style {
        let url = if region == "us-east-1" {
            format!("https://s3.amazonaws.com/{bucket}/{encoded_key}")
        } else {
            format!("https://s3.{region}.amazonaws.com/{bucket}/{encoded_key}")
        };
        return target_from_url(url, format!("/{bucket}/{encoded_key}"), Vec::new());
    }

    let url = if region == "us-east-1" {
        format!("https://{bucket}.s3.amazonaws.com/{encoded_key}")
    } else {
        format!("https://{bucket}.s3.{region}.amazonaws.com/{encoded_key}")
    };
    target_from_url(url, format!("/{encoded_key}"), Vec::new())
}

fn build_s3_list_target(
    config: &S3CacheConfig,
    prefix: Option<&str>,
    continuation_token: Option<&str>,
) -> Result<S3Target> {
    let bucket = config.bucket.trim();
    if bucket.is_empty() {
        bail!("cache.s3.bucket cannot be empty");
    }

    let mut query_pairs = vec![("list-type".to_string(), "2".to_string())];
    if let Some(prefix) = prefix.map(str::trim).filter(|value| !value.is_empty()) {
        query_pairs.push(("prefix".to_string(), prefix.to_string()));
    }
    if let Some(token) = continuation_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        query_pairs.push(("continuation-token".to_string(), token.to_string()));
    }
    let canonical_query = canonical_query_string(&query_pairs);

    if let Some(endpoint) = config.endpoint.as_deref()
        && !endpoint.trim().is_empty()
    {
        let endpoint = endpoint.trim().trim_end_matches('/');
        if config.path_style {
            let url = format!("{endpoint}/{bucket}?{canonical_query}");
            return target_from_url(url, format!("/{bucket}"), query_pairs);
        }
        let url = format!(
            "{}?{}",
            s3_virtual_host_url(endpoint, bucket, "").trim_end_matches('/'),
            canonical_query
        );
        return target_from_url(url, "/".to_string(), query_pairs);
    }

    let region = config
        .region
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("us-east-1");
    if config.path_style {
        let base = if region == "us-east-1" {
            format!("https://s3.amazonaws.com/{bucket}")
        } else {
            format!("https://s3.{region}.amazonaws.com/{bucket}")
        };
        return target_from_url(
            format!("{base}?{canonical_query}"),
            format!("/{bucket}"),
            query_pairs,
        );
    }

    let base = if region == "us-east-1" {
        format!("https://{bucket}.s3.amazonaws.com")
    } else {
        format!("https://{bucket}.s3.{region}.amazonaws.com")
    };
    target_from_url(
        format!("{base}?{canonical_query}"),
        "/".to_string(),
        query_pairs,
    )
}

fn target_from_url(
    url: String,
    canonical_uri: String,
    query_pairs: Vec<(String, String)>,
) -> Result<S3Target> {
    let parsed = Url::parse(&url).with_context(|| format!("invalid s3 url `{url}`"))?;
    let host = parsed
        .host_str()
        .with_context(|| format!("missing host in `{url}`"))?;
    let host_header = if let Some(port) = parsed.port() {
        format!("{host}:{port}")
    } else {
        host.to_string()
    };
    Ok(S3Target {
        url,
        host_header,
        canonical_uri,
        query_pairs,
    })
}

fn write_response_to_path(mut response: Response, destination: &Path) -> Result<()> {
    let mut output = File::create(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    response
        .copy_to(&mut output)
        .with_context(|| format!("failed to write {}", destination.display()))?;
    Ok(())
}

fn s3_cache_object_key(
    config: &S3CacheConfig,
    metadata: &LockMetadata,
    package: &LockedPackage,
) -> String {
    let mut segments = Vec::new();
    if let Some(prefix) = config.prefix.as_deref() {
        for segment in prefix
            .split('/')
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            segments.push(sanitize_path_segment(segment));
        }
    }

    segments.push(sanitize_path_segment(metadata.loader.kind.as_str()));
    segments.push(sanitize_path_segment(&metadata.minecraft));
    segments.push(sanitize_path_segment(&package.id));
    segments.push(sanitize_path_segment(&package.version));
    segments.push(sanitize_path_segment(
        package.file_name.trim().trim_start_matches('/'),
    ));
    segments.join("/")
}

fn sanitize_path_segment(value: &str) -> String {
    let normalized = value.trim();
    if normalized.is_empty() {
        return "_".to_string();
    }
    normalized
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn encode_uri_component(input: &str, preserve_slash: bool) -> String {
    let mut encoded = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        let ch = byte as char;
        let keep = ch.is_ascii_alphanumeric()
            || matches!(ch, '-' | '_' | '.' | '~')
            || (preserve_slash && ch == '/');
        if keep {
            encoded.push(ch);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn s3_virtual_host_url(endpoint: &str, bucket: &str, encoded_key: &str) -> String {
    let encoded_key = encoded_key.trim_start_matches('/');
    if let Some((scheme, rest)) = endpoint.split_once("://") {
        if encoded_key.is_empty() {
            return format!("{scheme}://{bucket}.{}", rest.trim_start_matches('/'));
        }
        return format!(
            "{scheme}://{bucket}.{}/{}",
            rest.trim_start_matches('/'),
            encoded_key
        );
    }
    if encoded_key.is_empty() {
        return format!("https://{bucket}.{}", endpoint.trim_start_matches('/'));
    }
    format!(
        "https://{bucket}.{}/{}",
        endpoint.trim_start_matches('/'),
        encoded_key
    )
}

fn fetch_package_to_path(
    package: &LockedPackage,
    project_root: &Path,
    client: &Client,
    destination: &Path,
) -> Result<()> {
    if package.download_url.eq_ignore_ascii_case("pending") {
        bail!(
            "package {} has pending download URL, run `mineconda lock` first",
            package.id
        );
    }

    match package.source {
        ModSource::Local => {
            let source_path = resolve_local_path(project_root, &package.download_url);
            if !source_path.exists() {
                bail!("local package source not found: {}", source_path.display());
            }
            fs::copy(&source_path, destination).with_context(|| {
                format!(
                    "failed to copy local package {} -> {}",
                    source_path.display(),
                    destination.display()
                )
            })?;
        }
        ModSource::Modrinth | ModSource::Curseforge | ModSource::Url | ModSource::S3 => {
            download_remote_with_retries(client, &package.download_url, destination)?;
        }
    }

    Ok(())
}

fn download_remote_with_retries(client: &Client, url: &str, destination: &Path) -> Result<()> {
    let retries = sync_download_retries();
    for attempt in 1..=retries {
        let result = download_remote_once(client, url, destination);
        match result {
            Ok(()) => return Ok(()),
            Err(err) if attempt < retries => {
                let _ = fs::remove_file(destination);
                eprintln!("warning: download attempt {attempt}/{retries} failed for {url}: {err}");
                sleep(Duration::from_millis(500 * attempt as u64));
            }
            Err(err) => return Err(err),
        }
    }

    unreachable!("sync download retries loop should always return")
}

fn download_remote_once(client: &Client, url: &str, destination: &Path) -> Result<()> {
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("failed to download {url}"))?
        .error_for_status()
        .with_context(|| format!("download returned error for {url}"))?;
    write_response_to_path(response, destination)
}

fn verify_existing_cache(package: &LockedPackage, cache_path: &Path) -> Result<bool> {
    if !cache_path.exists() {
        return Ok(false);
    }

    if let Some(expected_size) = package.file_size {
        let actual_size = fs::metadata(cache_path)
            .with_context(|| format!("failed to stat {}", cache_path.display()))?
            .len();
        if expected_size != actual_size {
            return Ok(false);
        }
    }

    if let Some(expected) = expected_sha256(package) {
        let (actual_sha256, _) = compute_sha256_and_size(cache_path)?;
        if actual_sha256 != expected {
            return Ok(false);
        }
    }

    Ok(true)
}

fn update_package_metadata(package: &mut LockedPackage, file_path: &Path) -> Result<bool> {
    let (sha256, size) = compute_sha256_and_size(file_path)?;
    let mut changed = false;

    if package.file_size != Some(size) {
        package.file_size = Some(size);
        changed = true;
    }

    if package.sha256 != sha256 {
        package.sha256 = sha256.clone();
        changed = true;
    }

    if package.hash(HashAlgorithm::Sha256) != Some(sha256.as_str()) {
        package.upsert_hash(HashAlgorithm::Sha256, sha256);
        changed = true;
    }

    Ok(changed)
}

fn canonicalize_cache_path(
    cache_root: &Path,
    package: &LockedPackage,
    current_path: &Path,
) -> Result<PathBuf> {
    let canonical_path = cache_path_for_package(cache_root, package);
    if canonical_path == current_path {
        return Ok(current_path.to_path_buf());
    }

    if let Some(parent) = canonical_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create cache dir {}", parent.display()))?;
    }

    if canonical_path.exists() {
        if verify_existing_cache(package, &canonical_path)? {
            if current_path.exists() && current_path != canonical_path {
                fs::remove_file(current_path).with_context(|| {
                    format!(
                        "failed to remove stale cache file {}",
                        current_path.display()
                    )
                })?;
            }
            return Ok(canonical_path);
        }
        fs::remove_file(&canonical_path)
            .with_context(|| format!("failed to remove stale {}", canonical_path.display()))?;
    }

    if let Err(rename_error) = fs::rename(current_path, &canonical_path) {
        fs::copy(current_path, &canonical_path).with_context(|| {
            format!(
                "failed to move cache {} -> {}",
                current_path.display(),
                canonical_path.display()
            )
        })?;
        fs::remove_file(current_path)
            .with_context(|| format!("failed to remove {}", current_path.display()))?;
        if !canonical_path.exists() {
            bail!(
                "failed to move {} into canonical cache path: {}",
                current_path.display(),
                rename_error
            );
        }
    }

    Ok(canonical_path)
}

fn install_package_file(cache_path: &Path, target_path: &Path) -> Result<()> {
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    if target_path.exists() {
        fs::remove_file(target_path)
            .with_context(|| format!("failed to remove existing {}", target_path.display()))?;
    }

    if fs::hard_link(cache_path, target_path).is_err() {
        fs::copy(cache_path, target_path).with_context(|| {
            format!(
                "failed to install {} -> {}",
                cache_path.display(),
                target_path.display()
            )
        })?;
    }

    Ok(())
}

fn package_install_relative_path(package: &LockedPackage) -> Result<PathBuf> {
    normalize_relative_install_path(&package.install_path_or_default())
}

fn normalize_relative_install_path(raw: &str) -> Result<PathBuf> {
    if raw.trim().is_empty() {
        bail!("package install path must not be empty");
    }
    if raw.starts_with('/') || raw.starts_with('\\') {
        bail!("package install path must be relative: {raw}");
    }
    if raw.contains('\\') {
        bail!("package install path must use forward slashes: {raw}");
    }

    let mut normalized = PathBuf::new();
    for component in Path::new(raw).components() {
        match component {
            Component::Normal(value) => normalized.push(value),
            Component::CurDir => {}
            Component::ParentDir => {
                bail!("package install path must not contain parent traversal: {raw}")
            }
            Component::Prefix(_) | Component::RootDir => {
                bail!("package install path must be relative: {raw}")
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        bail!("package install path must not be empty");
    }

    Ok(normalized)
}

fn mods_root_file_name(relative_path: &Path) -> Option<String> {
    let mut components = relative_path.components();
    let first = components.next()?;
    let second = components.next()?;
    if components.next().is_some() {
        return None;
    }
    match first {
        Component::Normal(value) if value == "mods" => {}
        _ => return None,
    }
    match second {
        Component::Normal(name) => Some(name.to_string_lossy().to_string()),
        _ => None,
    }
}

fn prune_mods_directory(mods_dir: &Path, expected_files: &HashSet<String>) -> Result<usize> {
    let mut removed = 0;

    for entry in
        fs::read_dir(mods_dir).with_context(|| format!("failed to read {}", mods_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }

        let name = entry.file_name();
        let name = name.to_string_lossy().to_string();
        if name == ".gitkeep" {
            continue;
        }

        if !expected_files.contains(&name) {
            fs::remove_file(entry.path()).with_context(|| {
                format!("failed to remove stale mod file {}", entry.path().display())
            })?;
            removed += 1;
        }
    }

    Ok(removed)
}

fn cache_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("MINECONDA_CACHE_DIR") {
        return Ok(PathBuf::from(path));
    }

    if let Some(home) = env::var_os("MINECONDA_HOME") {
        return Ok(PathBuf::from(home).join("cache").join("mods"));
    }

    let home = env::var_os("HOME").context("HOME is not set and MINECONDA_HOME is missing")?;
    Ok(PathBuf::from(home)
        .join(".mineconda")
        .join("cache")
        .join("mods"))
}

fn sync_download_retries() -> usize {
    env::var("MINECONDA_SYNC_RETRIES")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(3)
}

fn cache_path_for_package(cache_root: &Path, package: &LockedPackage) -> PathBuf {
    let extension = Path::new(&package.file_name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("jar");
    let file_name = format!(
        "{}-{}.{}",
        sanitize_name(&package.id),
        sanitize_name(&package.cache_key()),
        extension
    );
    cache_root.join(file_name)
}

fn expected_cache_paths(lock: Option<&Lockfile>, cache_root: &Path) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    let Some(lock) = lock else {
        return out;
    };

    for package in &lock.packages {
        out.insert(cache_path_for_package(cache_root, package));
    }
    out
}

pub fn cache_root_path() -> Result<PathBuf> {
    cache_root()
}

pub fn cache_path_for_package_in(cache_root: &Path, package: &LockedPackage) -> PathBuf {
    cache_path_for_package(cache_root, package)
}

fn resolve_local_path(project_root: &Path, raw: &str) -> PathBuf {
    let without_prefix = raw.strip_prefix("file://").unwrap_or(raw);
    let path = PathBuf::from(without_prefix);
    if path.is_absolute() {
        path
    } else {
        project_root.join(path)
    }
}

fn expected_sha256(package: &LockedPackage) -> Option<String> {
    if !package.sha256.is_empty() && package.sha256 != "pending" {
        return Some(package.sha256.clone());
    }

    package
        .hash(HashAlgorithm::Sha256)
        .map(std::string::ToString::to_string)
}

fn compute_sha256_and_size(path: &Path) -> Result<(String, u64)> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        total += read as u64;
        hasher.update(&buffer[..read]);
    }

    let digest = hex::encode(hasher.finalize());
    Ok((digest, total))
}

fn sanitize_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

mod hex {
    pub fn encode<T: AsRef<[u8]>>(input: T) -> String {
        input
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mineconda_core::{LoaderKind, LoaderSpec, ModSide};

    fn sample_package() -> LockedPackage {
        LockedPackage {
            id: "ferrite-core".to_string(),
            source: ModSource::Modrinth,
            version: "6.0.1+1.21.1".to_string(),
            side: ModSide::Both,
            file_name: "ferrite core.jar".to_string(),
            install_path: None,
            file_size: None,
            sha256: "pending".to_string(),
            download_url: "https://example.com/ferrite-core.jar".to_string(),
            hashes: Vec::new(),
            source_ref: None,
            dependencies: Vec::new(),
        }
    }

    fn sample_metadata() -> LockMetadata {
        LockMetadata {
            generated_by: "mineconda-test".to_string(),
            generated_at_unix: 0,
            minecraft: "1.21.1".to_string(),
            loader: LoaderSpec {
                kind: LoaderKind::NeoForge,
                version: "latest".to_string(),
            },
            dependency_graph: true,
        }
    }

    fn sample_s3_config() -> S3CacheConfig {
        S3CacheConfig {
            enabled: true,
            bucket: "demo".to_string(),
            region: Some("us-east-1".to_string()),
            endpoint: Some("http://127.0.0.1:9000".to_string()),
            public_base_url: None,
            prefix: Some("cache".to_string()),
            path_style: true,
            access_key_env: Some("MINECONDA_TEST_ACCESS".to_string()),
            secret_key_env: Some("MINECONDA_TEST_SECRET".to_string()),
            session_token_env: Some("MINECONDA_TEST_TOKEN".to_string()),
            auth: S3CacheAuth::Sigv4,
            upload_enabled: true,
        }
    }

    #[test]
    fn s3_cache_key_includes_prefix_loader_and_version() {
        let config = sample_s3_config();
        let key = s3_cache_object_key(&config, &sample_metadata(), &sample_package());
        assert_eq!(
            key,
            "cache/neoforge/1.21.1/ferrite-core/6.0.1_1.21.1/ferrite_core.jar"
        );
    }

    #[test]
    fn object_target_prefers_public_base_for_anonymous() {
        let mut config = sample_s3_config();
        config.auth = S3CacheAuth::Anonymous;
        config.public_base_url = Some("https://cdn.example.com/demo".to_string());
        let target =
            build_s3_object_target(&config, "cache/mod.jar", false).expect("target should build");
        assert_eq!(target.url, "https://cdn.example.com/demo/cache/mod.jar");
    }

    #[test]
    fn object_target_ignores_public_base_for_signed_requests() {
        let mut config = sample_s3_config();
        config.public_base_url = Some("https://cdn.example.com/demo".to_string());
        let target =
            build_s3_object_target(&config, "cache/mod.jar", true).expect("target should build");
        assert_eq!(target.url, "http://127.0.0.1:9000/demo/cache/mod.jar");
    }

    #[test]
    fn list_target_uses_path_style_endpoint_with_encoded_prefix() {
        let config = sample_s3_config();
        let target = build_s3_list_target(&config, Some("prune/test"), Some("token/1"))
            .expect("list target should build");
        assert_eq!(
            target.url,
            "http://127.0.0.1:9000/demo?continuation-token=token%2F1&list-type=2&prefix=prune%2Ftest"
        );
        assert_eq!(target.canonical_uri, "/demo");
        assert_eq!(
            target.query_pairs,
            vec![
                ("list-type".to_string(), "2".to_string()),
                ("prefix".to_string(), "prune/test".to_string()),
                ("continuation-token".to_string(), "token/1".to_string())
            ]
        );
    }

    #[test]
    fn auto_auth_falls_back_to_anonymous_without_credentials() {
        let mut config = sample_s3_config();
        config.auth = S3CacheAuth::Auto;
        config.access_key_env = Some("MISSING_ACCESS".to_string());
        config.secret_key_env = Some("MISSING_SECRET".to_string());
        let backend = configure_s3_cache_backend(Some(&config))
            .expect("backend should configure")
            .expect("backend should exist");
        assert!(matches!(backend.auth, ResolvedS3Auth::Anonymous));
    }

    #[test]
    fn sigv4_signing_contains_expected_headers() {
        let credentials = S3Credentials {
            access_key: "test-access".to_string(),
            secret_key: "test-secret".to_string(),
            session_token: Some("test-token".to_string()),
            region: "us-east-1".to_string(),
        };
        let target = S3Target {
            url: "http://127.0.0.1:9000/demo/cache/mod.jar".to_string(),
            host_header: "127.0.0.1:9000".to_string(),
            canonical_uri: "/demo/cache/mod.jar".to_string(),
            query_pairs: vec![("list-type".to_string(), "2".to_string())],
        };
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("timestamp");
        let signed =
            sign_s3_request(&Method::GET, &target, b"", &credentials, now).expect("signed");
        assert!(
            signed
                .authorization
                .starts_with("AWS4-HMAC-SHA256 Credential=test-access/")
        );
        assert_eq!(signed.host_header, "127.0.0.1:9000");
        assert_eq!(signed.payload_sha256, hex_sha256(b""));
        assert_eq!(signed.session_token.as_deref(), Some("test-token"));
    }

    #[test]
    fn verify_report_marks_missing_cache_for_lock_packages() {
        let temp_root = env::temp_dir().join(format!("mineconda-sync-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_root);
        fs::create_dir_all(&temp_root).expect("temp dir");

        let lock = Lockfile {
            metadata: sample_metadata(),
            packages: vec![sample_package()],
        };
        let report = verify_cache_entries(Some(&lock), &temp_root, false).expect("verify");
        assert_eq!(report.missing, 1);
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn stats_report_splits_referenced_and_unreferenced_files() {
        let temp_root =
            env::temp_dir().join(format!("mineconda-sync-stats-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_root);
        fs::create_dir_all(&temp_root).expect("temp dir");

        let package = sample_package();
        let lock = Lockfile {
            metadata: sample_metadata(),
            packages: vec![package.clone()],
        };
        let referenced_path = cache_path_for_package(&temp_root, &package);
        fs::write(&referenced_path, b"hello").expect("write referenced");
        fs::write(temp_root.join("extra.jar"), b"world").expect("write extra");

        let stats = collect_cache_stats(Some(&lock), &temp_root).expect("stats");
        assert_eq!(stats.file_count, 2);
        assert_eq!(stats.referenced_files, 1);
        assert_eq!(stats.unreferenced_files, 1);
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn prune_candidate_partition_respects_cutoff() {
        let cutoff = OffsetDateTime::parse("2024-01-01T00:00:00Z", &Rfc3339).expect("cutoff");
        let items = vec![
            ListBucketItem {
                key: "prune/old.jar".to_string(),
                last_modified: "2020-01-01T00:00:00Z".to_string(),
                _size: 3,
            },
            ListBucketItem {
                key: "prune/new.jar".to_string(),
                last_modified: "2099-01-01T00:00:00Z".to_string(),
                _size: 3,
            },
        ];

        let (candidates, retained) =
            partition_prune_candidates(items, cutoff).expect("partition should succeed");
        assert_eq!(retained, 1);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].key, "prune/old.jar");
    }
}
