use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use mineconda_core::{JavaProvider, http_user_agent};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use tar::Archive;
use zip::ZipArchive;

const ADOPTIUM_API_BASE: &str = "https://api.adoptium.net/v3/assets/latest";

#[derive(Debug, Clone)]
pub struct InstalledJavaRuntime {
    pub version: String,
    pub provider: JavaProvider,
    pub java_bin: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct RuntimeMetadata {
    version: String,
    provider: JavaProvider,
    java_home: String,
}

#[derive(Debug, Deserialize)]
struct AdoptiumAsset {
    binary: AdoptiumBinary,
}

#[derive(Debug, Deserialize)]
struct AdoptiumBinary {
    package: AdoptiumPackage,
}

#[derive(Debug, Deserialize)]
struct AdoptiumPackage {
    name: String,
    link: String,
}

pub fn mineconda_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("MINECONDA_HOME") {
        return Ok(PathBuf::from(path));
    }

    let home = env::var_os("HOME").context("HOME is not set and MINECONDA_HOME is missing")?;
    Ok(PathBuf::from(home).join(".mineconda"))
}

pub fn java_runtime_root() -> Result<PathBuf> {
    Ok(mineconda_home()?.join("runtimes").join("java"))
}

pub fn resolve_java_binary(
    version: &str,
    provider: JavaProvider,
    auto_install: bool,
) -> Result<PathBuf> {
    if let Some(path) = find_java_runtime(version, provider)? {
        return Ok(path);
    }

    if auto_install {
        return ensure_java_runtime(version, provider, false);
    }

    bail!(
        "java {} ({}) is not installed. run `mineconda env install {}` first",
        version,
        provider.as_str(),
        version
    )
}

pub fn ensure_java_runtime(version: &str, provider: JavaProvider, force: bool) -> Result<PathBuf> {
    if !force && let Some(path) = find_java_runtime(version, provider)? {
        return Ok(path);
    }

    if provider != JavaProvider::Temurin {
        bail!("unsupported java provider: {}", provider.as_str());
    }

    let install_dir = install_dir(version, provider)?;
    if force && install_dir.exists() {
        fs::remove_dir_all(&install_dir)
            .with_context(|| format!("failed to clean old runtime at {}", install_dir.display()))?;
    }
    fs::create_dir_all(&install_dir)
        .with_context(|| format!("failed to create {}", install_dir.display()))?;

    install_from_adoptium(version, &install_dir)?;

    let payload = payload_dir(&install_dir);
    let java_home = detect_java_home(&payload, 0)?.with_context(|| {
        format!(
            "java executable was not found after extraction under {}",
            payload.display()
        )
    })?;

    let java_bin = java_home.join("bin").join(java_executable_name());
    let metadata = RuntimeMetadata {
        version: version.to_string(),
        provider,
        java_home: java_home.display().to_string(),
    };
    let metadata_raw = serde_json::to_string_pretty(&metadata)?;
    fs::write(metadata_path(&install_dir), metadata_raw)?;

    Ok(java_bin)
}

pub fn find_java_runtime(version: &str, provider: JavaProvider) -> Result<Option<PathBuf>> {
    let install_dir = install_dir(version, provider)?;
    if !install_dir.exists() {
        return Ok(None);
    }

    if let Some(path) = java_bin_from_metadata(&install_dir)? {
        return Ok(Some(path));
    }

    let payload = payload_dir(&install_dir);
    if !payload.exists() {
        return Ok(None);
    }

    let Some(java_home) = detect_java_home(&payload, 0)? else {
        return Ok(None);
    };

    let java_bin = java_home.join("bin").join(java_executable_name());
    if !java_bin.exists() {
        return Ok(None);
    }

    let metadata = RuntimeMetadata {
        version: version.to_string(),
        provider,
        java_home: java_home.display().to_string(),
    };
    fs::write(
        metadata_path(&install_dir),
        serde_json::to_string_pretty(&metadata)?,
    )?;
    Ok(Some(java_bin))
}

pub fn list_java_runtimes() -> Result<Vec<InstalledJavaRuntime>> {
    let root = java_runtime_root()?;
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut runtimes = Vec::new();
    for provider_entry in fs::read_dir(&root)? {
        let provider_entry = provider_entry?;
        if !provider_entry.file_type()?.is_dir() {
            continue;
        }

        let provider_name = provider_entry.file_name().to_string_lossy().to_string();
        let Some(provider) = parse_provider(&provider_name) else {
            continue;
        };

        for version_entry in fs::read_dir(provider_entry.path())? {
            let version_entry = version_entry?;
            if !version_entry.file_type()?.is_dir() {
                continue;
            }

            let version = version_entry.file_name().to_string_lossy().to_string();
            if let Some(java_bin) = find_java_runtime(&version, provider)? {
                runtimes.push(InstalledJavaRuntime {
                    version,
                    provider,
                    java_bin,
                });
            }
        }
    }

    runtimes.sort_by(|a, b| a.version.cmp(&b.version));
    Ok(runtimes)
}

fn install_from_adoptium(version: &str, install_dir: &Path) -> Result<()> {
    let (os, arch) = platform()?;
    let client = Client::builder()
        .user_agent(http_user_agent())
        .build()
        .context("failed to build HTTP client")?;

    let endpoint = format!("{ADOPTIUM_API_BASE}/{version}/hotspot");
    let response = client
        .get(endpoint)
        .query(&[
            ("architecture", arch),
            ("heap_size", "normal"),
            ("image_type", "jdk"),
            ("os", os),
            ("vendor", "eclipse"),
        ])
        .send()
        .context("failed to query Adoptium API")?
        .error_for_status()
        .context("Adoptium API returned an error status")?;

    let assets: Vec<AdoptiumAsset> = response
        .json()
        .context("failed to decode Adoptium API response")?;
    let package = assets
        .into_iter()
        .next()
        .context("no matching Java runtime found from Adoptium")?
        .binary
        .package;

    let downloads = install_dir.join("downloads");
    fs::create_dir_all(&downloads)?;
    let archive_path = downloads.join(package.name);
    let mut archive_file = File::create(&archive_path)?;

    let mut download = client
        .get(package.link)
        .send()
        .context("failed to download Java runtime archive")?
        .error_for_status()
        .context("Java runtime archive download returned an error")?;

    download
        .copy_to(&mut archive_file)
        .context("failed to write Java archive")?;
    archive_file.flush()?;

    let payload = payload_dir(install_dir);
    if payload.exists() {
        fs::remove_dir_all(&payload)
            .with_context(|| format!("failed to remove old payload {}", payload.display()))?;
    }
    fs::create_dir_all(&payload)?;

    extract_archive(&archive_path, &payload)?;
    Ok(())
}

fn extract_archive(archive_path: &Path, destination: &Path) -> Result<()> {
    let name = archive_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if name.ends_with(".zip") {
        let file = File::open(archive_path)?;
        let mut archive = ZipArchive::new(file).context("failed to open zip archive")?;
        archive
            .extract(destination)
            .context("failed to extract zip archive")?;
        return Ok(());
    }

    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        let file = File::open(archive_path)?;
        let decoder = GzDecoder::new(file);
        let mut archive = Archive::new(decoder);
        archive
            .unpack(destination)
            .context("failed to extract tar.gz archive")?;
        return Ok(());
    }

    if name.ends_with(".tar") {
        let file = File::open(archive_path)?;
        let mut archive = Archive::new(file);
        archive
            .unpack(destination)
            .context("failed to extract tar archive")?;
        return Ok(());
    }

    bail!(
        "unsupported Java archive format: {}",
        archive_path.display()
    )
}

fn detect_java_home(root: &Path, depth: usize) -> Result<Option<PathBuf>> {
    if depth > 8 || !root.exists() {
        return Ok(None);
    }

    let direct = root.join("bin").join(java_executable_name());
    if direct.exists() {
        return Ok(Some(root.to_path_buf()));
    }

    let mac_bundle = root
        .join("Contents")
        .join("Home")
        .join("bin")
        .join(java_executable_name());
    if mac_bundle.exists() {
        return Ok(Some(root.join("Contents").join("Home")));
    }

    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(path) = detect_java_home(&entry.path(), depth + 1)? {
            return Ok(Some(path));
        }
    }

    Ok(None)
}

fn java_bin_from_metadata(install_dir: &Path) -> Result<Option<PathBuf>> {
    let metadata_path = metadata_path(install_dir);
    if !metadata_path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(&metadata_path)
        .with_context(|| format!("failed to read {}", metadata_path.display()))?;
    let metadata: RuntimeMetadata = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", metadata_path.display()))?;

    let java_bin = PathBuf::from(metadata.java_home)
        .join("bin")
        .join(java_executable_name());
    if java_bin.exists() {
        Ok(Some(java_bin))
    } else {
        Ok(None)
    }
}

fn install_dir(version: &str, provider: JavaProvider) -> Result<PathBuf> {
    Ok(java_runtime_root()?.join(provider.as_str()).join(version))
}

fn metadata_path(install_dir: &Path) -> PathBuf {
    install_dir.join("runtime.json")
}

fn payload_dir(install_dir: &Path) -> PathBuf {
    install_dir.join("payload")
}

fn parse_provider(name: &str) -> Option<JavaProvider> {
    match name {
        "temurin" => Some(JavaProvider::Temurin),
        _ => None,
    }
}

fn java_executable_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "java.exe"
    } else {
        "java"
    }
}

fn platform() -> Result<(&'static str, &'static str)> {
    let os = if cfg!(target_os = "macos") {
        "mac"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        bail!("unsupported operating system for runtime installation");
    };

    let arch = if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        bail!("unsupported architecture for runtime installation");
    };

    Ok((os, arch))
}
