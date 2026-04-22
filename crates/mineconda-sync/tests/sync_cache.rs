use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use mineconda_core::{
    HashAlgorithm, LoaderKind, LoaderSpec, LockMetadata, LockedPackage, ModSide, ModSource,
    PackageHash,
};
use mineconda_sync::{
    SyncRequest, cache_path_for_package_in, collect_cache_stats, sync_lockfile,
    verify_cache_entries,
};
use sha2::{Digest, Sha256};

fn env_lock() -> &'static Mutex<()> {
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    ENV_LOCK.get_or_init(|| Mutex::new(()))
}

struct ScopedEnv {
    key: &'static str,
    previous: Option<OsString>,
}

impl ScopedEnv {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = env::var_os(key);
        unsafe {
            env::set_var(key, value);
        }
        Self { key, previous }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        match self.previous.as_ref() {
            Some(value) => unsafe {
                env::set_var(self.key, value);
            },
            None => unsafe {
                env::remove_var(self.key);
            },
        }
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time drift")
        .as_nanos();
    let path = env::temp_dir().join(format!("mineconda-sync-{label}-{}-{nonce}", process::id()));
    fs::create_dir_all(&path).expect("failed to create temp dir");
    path
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sample_package(project_root: &Path, name: &str, bytes: &[u8]) -> LockedPackage {
    let vendor_dir = project_root.join("vendor");
    fs::create_dir_all(&vendor_dir).expect("failed to create vendor dir");
    let relative = format!("vendor/{name}.jar");
    fs::write(project_root.join(&relative), bytes).expect("failed to write vendor jar");
    let sha256 = sha256_hex(bytes);

    LockedPackage {
        id: name.to_string(),
        source: ModSource::Local,
        version: "1.0.0".to_string(),
        side: ModSide::Both,
        file_name: format!("{name}.jar"),
        install_path: None,
        file_size: Some(bytes.len() as u64),
        sha256: sha256.clone(),
        download_url: relative,
        hashes: vec![PackageHash {
            algorithm: HashAlgorithm::Sha256,
            value: sha256,
        }],
        source_ref: None,
        groups: vec!["default".to_string()],
        dependencies: Vec::new(),
    }
}

fn sample_lockfile(packages: Vec<LockedPackage>) -> mineconda_core::Lockfile {
    mineconda_core::Lockfile {
        metadata: LockMetadata {
            generated_by: "test".to_string(),
            generated_at_unix: 1,
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

fn pending_local_package(project_root: &Path, name: &str, bytes: &[u8]) -> LockedPackage {
    let vendor_dir = project_root.join("vendor");
    fs::create_dir_all(&vendor_dir).expect("failed to create vendor dir");
    let relative = format!("vendor/{name}.jar");
    fs::write(project_root.join(&relative), bytes).expect("failed to write vendor jar");

    LockedPackage {
        id: name.to_string(),
        source: ModSource::Local,
        version: relative.clone(),
        side: ModSide::Both,
        file_name: format!("{name}.jar"),
        install_path: None,
        file_size: None,
        sha256: "pending".to_string(),
        download_url: relative,
        hashes: Vec::new(),
        source_ref: None,
        groups: vec!["default".to_string()],
        dependencies: Vec::new(),
    }
}

#[test]
fn sync_offline_uses_warmed_cache_with_multiple_jobs() -> Result<()> {
    let _env_guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
    let project_root = temp_dir("offline-warm");
    let cache_root = temp_dir("offline-cache");
    let _cache_env = ScopedEnv::set(
        "MINECONDA_CACHE_DIR",
        cache_root.to_str().expect("cache path must be utf-8"),
    );

    let package_a = sample_package(&project_root, "alpha", b"alpha-cache");
    let package_b = sample_package(&project_root, "beta", b"beta-cache");
    let mut lock = sample_lockfile(vec![package_a.clone(), package_b.clone()]);

    let first = sync_lockfile(
        &mut lock,
        &SyncRequest {
            project_root: project_root.clone(),
            prune: true,
            s3_cache: None,
            offline: false,
            jobs: 2,
            verbose_cache: false,
        },
    )?;
    assert_eq!(first.origin_downloads, 2);
    assert_eq!(first.network_attempts, 0);
    assert_eq!(first.installed, 2);

    fs::remove_file(project_root.join("vendor/alpha.jar"))?;
    fs::remove_file(project_root.join("vendor/beta.jar"))?;
    fs::remove_file(project_root.join("mods/alpha.jar"))?;
    fs::remove_file(project_root.join("mods/beta.jar"))?;

    let second = sync_lockfile(
        &mut lock,
        &SyncRequest {
            project_root: project_root.clone(),
            prune: true,
            s3_cache: None,
            offline: true,
            jobs: 2,
            verbose_cache: false,
        },
    )?;
    assert_eq!(second.local_hits, 2);
    assert_eq!(second.origin_downloads, 0);
    assert_eq!(second.network_attempts, 0);
    assert!(project_root.join("mods/alpha.jar").exists());
    assert!(project_root.join("mods/beta.jar").exists());
    Ok(())
}

#[test]
fn sync_installs_package_to_custom_install_path() -> Result<()> {
    let _env_guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
    let project_root = temp_dir("custom-install-path");
    let cache_root = temp_dir("custom-install-cache");
    let _cache_env = ScopedEnv::set(
        "MINECONDA_CACHE_DIR",
        cache_root.to_str().expect("cache path must be utf-8"),
    );

    let mut package = sample_package(&project_root, "theta", b"theta-cache");
    package.install_path = Some("config/imported/theta.toml".to_string());
    let mut lock = sample_lockfile(vec![package.clone()]);

    let report = sync_lockfile(
        &mut lock,
        &SyncRequest {
            project_root: project_root.clone(),
            prune: true,
            s3_cache: None,
            offline: false,
            jobs: 1,
            verbose_cache: false,
        },
    )?;
    assert_eq!(report.installed, 1);
    assert!(project_root.join("config/imported/theta.toml").exists());
    assert!(!project_root.join("mods/theta.jar").exists());
    Ok(())
}

#[test]
fn sync_offline_fails_when_cache_is_missing() {
    let _env_guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
    let project_root = temp_dir("offline-miss");
    let cache_root = temp_dir("offline-miss-cache");
    let _cache_env = ScopedEnv::set(
        "MINECONDA_CACHE_DIR",
        cache_root.to_str().expect("cache path must be utf-8"),
    );

    let package = sample_package(&project_root, "gamma", b"gamma-cache");
    let mut lock = sample_lockfile(vec![package]);
    let err = sync_lockfile(
        &mut lock,
        &SyncRequest {
            project_root,
            prune: true,
            s3_cache: None,
            offline: true,
            jobs: 1,
            verbose_cache: false,
        },
    )
    .expect_err("offline sync should fail without cache");

    assert!(
        err.to_string()
            .contains("offline sync cannot fetch missing cache")
    );
}

#[test]
fn verify_repair_removes_corrupted_cache_and_stats_track_references() -> Result<()> {
    let _env_guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
    let project_root = temp_dir("verify-repair");
    let cache_root = temp_dir("verify-cache");
    let _cache_env = ScopedEnv::set(
        "MINECONDA_CACHE_DIR",
        cache_root.to_str().expect("cache path must be utf-8"),
    );

    let package = sample_package(&project_root, "delta", b"delta-cache");
    let mut lock = sample_lockfile(vec![package.clone()]);
    sync_lockfile(
        &mut lock,
        &SyncRequest {
            project_root: project_root.clone(),
            prune: true,
            s3_cache: None,
            offline: false,
            jobs: 1,
            verbose_cache: false,
        },
    )?;

    let stats = collect_cache_stats(Some(&lock), &cache_root)?;
    assert_eq!(stats.file_count, 1);
    assert_eq!(stats.referenced_files, 1);
    assert_eq!(stats.unreferenced_files, 0);

    let cache_path = cache_path_for_package_in(&cache_root, &package);
    fs::write(&cache_path, b"corrupted")?;

    let report = verify_cache_entries(Some(&lock), &cache_root, true)?;
    assert_eq!(report.checked, 1);
    assert_eq!(report.invalid, 1);
    assert_eq!(report.repaired, 1);
    assert!(!cache_path.exists());
    Ok(())
}

#[test]
fn sync_promotes_pending_cache_entry_to_hashed_cache_key() -> Result<()> {
    let _env_guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
    let project_root = temp_dir("pending-cache");
    let cache_root = temp_dir("pending-cache-root");
    let _cache_env = ScopedEnv::set(
        "MINECONDA_CACHE_DIR",
        cache_root.to_str().expect("cache path must be utf-8"),
    );

    let package = pending_local_package(&project_root, "epsilon", b"epsilon-cache");
    let initial_cache_path = cache_path_for_package_in(&cache_root, &package);
    let mut lock = sample_lockfile(vec![package]);

    let report = sync_lockfile(
        &mut lock,
        &SyncRequest {
            project_root: project_root.clone(),
            prune: true,
            s3_cache: None,
            offline: false,
            jobs: 1,
            verbose_cache: false,
        },
    )?;
    assert_eq!(report.origin_downloads, 1);
    assert_eq!(report.network_attempts, 0);

    let package = &lock.packages[0];
    let hashed_cache_path = cache_path_for_package_in(&cache_root, package);
    assert_ne!(initial_cache_path, hashed_cache_path);
    assert!(!initial_cache_path.exists());
    assert!(hashed_cache_path.exists());

    let stats = collect_cache_stats(Some(&lock), &cache_root)?;
    assert_eq!(stats.file_count, 1);
    assert_eq!(stats.referenced_files, 1);
    assert_eq!(stats.unreferenced_files, 0);
    Ok(())
}
