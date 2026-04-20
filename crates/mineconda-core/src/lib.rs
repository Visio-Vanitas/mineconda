use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MANIFEST_FILE: &str = "mineconda.toml";
pub const LOCK_FILE: &str = "mineconda.lock";
pub const LOCK_GENERATOR: &str = "mineconda/0.1.0";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub project: ProjectSection,
    #[serde(default)]
    pub mods: Vec<ModSpec>,
    #[serde(default, skip_serializing_if = "SourceRegistry::is_empty")]
    pub sources: SourceRegistry,
    #[serde(default, skip_serializing_if = "CacheRegistry::is_empty")]
    pub cache: CacheRegistry,
    #[serde(default)]
    pub server: ServerProfile,
    #[serde(default)]
    pub runtime: RuntimeProfile,
}

impl Manifest {
    pub fn new(
        name: String,
        minecraft: String,
        loader_kind: LoaderKind,
        loader_version: String,
    ) -> Self {
        Self {
            project: ProjectSection {
                name,
                minecraft,
                loader: LoaderSpec {
                    kind: loader_kind,
                    version: loader_version,
                },
            },
            mods: Vec::new(),
            sources: SourceRegistry::default(),
            cache: CacheRegistry::default(),
            server: ServerProfile::default(),
            runtime: RuntimeProfile::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSection {
    pub name: String,
    pub minecraft: String,
    pub loader: LoaderSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoaderSpec {
    pub kind: LoaderKind,
    pub version: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum LoaderKind {
    #[default]
    Fabric,
    Forge,
    NeoForge,
    Quilt,
}

impl LoaderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fabric => "fabric-loader",
            Self::Forge => "forge",
            Self::NeoForge => "neoforge",
            Self::Quilt => "quilt-loader",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModSpec {
    pub id: String,
    pub source: ModSource,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_path: Option<String>,
    #[serde(default)]
    pub side: ModSide,
}

impl ModSpec {
    pub fn new(id: String, source: ModSource, version: String, side: ModSide) -> Self {
        Self {
            id,
            source,
            version,
            install_path: None,
            side,
        }
    }

    pub fn install_path_or_default(&self) -> String {
        self.install_path
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("mods/{}", self.id))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModSource {
    Modrinth,
    Curseforge,
    Url,
    Local,
    S3,
}

impl ModSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Modrinth => "modrinth",
            Self::Curseforge => "curseforge",
            Self::Url => "url",
            Self::Local => "local",
            Self::S3 => "s3",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SourceRegistry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3SourceConfig>,
}

impl SourceRegistry {
    pub fn is_empty(&self) -> bool {
        self.s3.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3SourceConfig {
    pub bucket: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_prefix: Option<String>,
    #[serde(default)]
    pub path_style: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CacheRegistry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3CacheConfig>,
}

impl CacheRegistry {
    pub fn is_empty(&self) -> bool {
        self.s3.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3CacheConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub bucket: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default)]
    pub path_style: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token_env: Option<String>,
    #[serde(default)]
    pub auth: S3CacheAuth,
    #[serde(default = "default_enabled")]
    pub upload_enabled: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum S3CacheAuth {
    #[default]
    Auto,
    Anonymous,
    Sigv4,
}

impl S3CacheAuth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Anonymous => "anonymous",
            Self::Sigv4 => "sigv4",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModSide {
    #[default]
    Both,
    Client,
    Server,
}

impl ModSide {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Client => "client",
            Self::Server => "server",
        }
    }

    pub fn compatible_with(self, desired: ModSide) -> bool {
        matches!(self, Self::Both) || matches!(desired, Self::Both) || self == desired
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeProfile {
    #[serde(default = "default_java_runtime_version")]
    pub java: String,
    #[serde(default)]
    pub provider: JavaProvider,
    #[serde(default = "default_runtime_auto_install")]
    pub auto_install: bool,
}

impl Default for RuntimeProfile {
    fn default() -> Self {
        Self {
            java: default_java_runtime_version(),
            provider: JavaProvider::default(),
            auto_install: default_runtime_auto_install(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JavaProvider {
    #[default]
    Temurin,
}

impl JavaProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Temurin => "temurin",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerProfile {
    #[serde(default = "default_java")]
    pub java: String,
    #[serde(default = "default_memory")]
    pub memory: String,
    #[serde(default)]
    pub jvm_args: Vec<String>,
}

impl Default for ServerProfile {
    fn default() -> Self {
        Self {
            java: default_java(),
            memory: default_memory(),
            jvm_args: Vec::new(),
        }
    }
}

fn default_java() -> String {
    "java".to_string()
}

fn default_memory() -> String {
    "4G".to_string()
}

fn default_java_runtime_version() -> String {
    "21".to_string()
}

fn default_runtime_auto_install() -> bool {
    true
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lockfile {
    pub metadata: LockMetadata,
    #[serde(default)]
    pub packages: Vec<LockedPackage>,
}

impl Lockfile {
    pub fn from_packages(manifest: &Manifest, packages: Vec<LockedPackage>) -> Self {
        Self {
            metadata: LockMetadata {
                generated_by: LOCK_GENERATOR.to_string(),
                generated_at_unix: unix_timestamp(),
                minecraft: manifest.project.minecraft.clone(),
                loader: manifest.project.loader.clone(),
            },
            packages,
        }
    }

    pub fn normalize_hashes(&mut self) {
        for package in &mut self.packages {
            package.normalize_hashes();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockMetadata {
    pub generated_by: String,
    #[serde(default = "unix_timestamp")]
    pub generated_at_unix: u64,
    pub minecraft: String,
    pub loader: LoaderSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedPackage {
    pub id: String,
    pub source: ModSource,
    pub version: String,
    #[serde(default)]
    pub side: ModSide,
    pub file_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_path: Option<String>,
    #[serde(default)]
    pub file_size: Option<u64>,
    pub sha256: String,
    pub download_url: String,
    #[serde(default)]
    pub hashes: Vec<PackageHash>,
    #[serde(default)]
    pub source_ref: Option<String>,
}

impl LockedPackage {
    pub fn placeholder(mod_spec: &ModSpec) -> Self {
        Self {
            id: mod_spec.id.clone(),
            source: mod_spec.source,
            version: mod_spec.version.clone(),
            side: mod_spec.side,
            file_name: format!("{}-{}.jar", mod_spec.id, mod_spec.version),
            install_path: mod_spec.install_path.clone(),
            file_size: None,
            sha256: "pending".to_string(),
            download_url: "pending".to_string(),
            hashes: Vec::new(),
            source_ref: None,
        }
    }

    pub fn normalize_hashes(&mut self) {
        if !self.sha256.trim().is_empty()
            && self.sha256 != "pending"
            && self.hash(HashAlgorithm::Sha256).is_none()
        {
            self.hashes.push(PackageHash {
                algorithm: HashAlgorithm::Sha256,
                value: self.sha256.clone(),
            });
        }
    }

    pub fn upsert_hash(&mut self, algorithm: HashAlgorithm, value: String) {
        if let Some(existing) = self.hashes.iter_mut().find(|h| h.algorithm == algorithm) {
            existing.value = value.clone();
        } else {
            self.hashes.push(PackageHash {
                algorithm,
                value: value.clone(),
            });
        }

        if algorithm == HashAlgorithm::Sha256 {
            self.sha256 = value;
        }
    }

    pub fn hash(&self, algorithm: HashAlgorithm) -> Option<&str> {
        self.hashes
            .iter()
            .find(|entry| entry.algorithm == algorithm)
            .map(|entry| entry.value.as_str())
    }

    pub fn cache_key(&self) -> String {
        if let Some(value) = self.hash(HashAlgorithm::Sha512) {
            return format!("sha512-{value}");
        }

        if let Some(value) = self.hash(HashAlgorithm::Sha1) {
            return format!("sha1-{value}");
        }

        if let Some(value) = self.hash(HashAlgorithm::Sha256) {
            return format!("sha256-{value}");
        }

        format!(
            "{}-{}-{}",
            sanitize_name(&self.id),
            sanitize_name(&self.version),
            self.source.as_str()
        )
    }

    pub fn install_path_or_default(&self) -> String {
        self.install_path
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("mods/{}", self.file_name))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageHash {
    pub algorithm: HashAlgorithm,
    pub value: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HashAlgorithm {
    Sha1,
    Sha256,
    Sha512,
    Md5,
}

impl HashAlgorithm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
            Self::Md5 => "md5",
        }
    }
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TOML parse error: {0}")]
    TomlDe(#[from] toml::de::Error),
    #[error("TOML serialize error: {0}")]
    TomlSer(#[from] toml::ser::Error),
}

pub fn manifest_path(root: &Path) -> PathBuf {
    root.join(MANIFEST_FILE)
}

pub fn lockfile_path(root: &Path) -> PathBuf {
    root.join(LOCK_FILE)
}

pub fn read_manifest(path: &Path) -> Result<Manifest, CoreError> {
    let raw = fs::read_to_string(path)?;
    Ok(toml::from_str(&raw)?)
}

pub fn write_manifest(path: &Path, manifest: &Manifest) -> Result<(), CoreError> {
    let raw = toml::to_string_pretty(manifest)?;
    fs::write(path, raw)?;
    Ok(())
}

pub fn read_lockfile(path: &Path) -> Result<Lockfile, CoreError> {
    let raw = fs::read_to_string(path)?;
    let mut lockfile: Lockfile = toml::from_str(&raw)?;
    lockfile.normalize_hashes();
    Ok(lockfile)
}

pub fn write_lockfile(path: &Path, lockfile: &Lockfile) -> Result<(), CoreError> {
    let mut normalized = lockfile.clone();
    normalized.normalize_hashes();
    let raw = toml::to_string_pretty(&normalized)?;
    fs::write(path, raw)?;
    Ok(())
}

pub fn build_lockfile_from_manifest(manifest: &Manifest) -> Lockfile {
    let packages = manifest
        .mods
        .iter()
        .map(LockedPackage::placeholder)
        .collect();
    Lockfile::from_packages(manifest, packages)
}

fn unix_timestamp() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs(),
        Err(_) => 0,
    }
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

#[cfg(test)]
mod tests {
    use super::{Manifest, S3CacheAuth};

    #[test]
    fn s3_cache_auth_defaults_to_auto() {
        let manifest: Manifest = toml::from_str(
            r#"
[project]
name = "pack"
minecraft = "1.21.1"

[project.loader]
kind = "neo-forge"
version = "latest"

[cache.s3]
enabled = true
bucket = "demo"
"#,
        )
        .expect("manifest should parse");
        let cache = manifest.cache.s3.expect("cache.s3 should exist");
        assert_eq!(cache.auth, S3CacheAuth::Auto);
    }

    #[test]
    fn s3_cache_auth_parses_sigv4() {
        let manifest: Manifest = toml::from_str(
            r#"
[project]
name = "pack"
minecraft = "1.21.1"

[project.loader]
kind = "neo-forge"
version = "latest"

[cache.s3]
enabled = true
bucket = "demo"
auth = "sigv4"
"#,
        )
        .expect("manifest should parse");
        let cache = manifest.cache.s3.expect("cache.s3 should exist");
        assert_eq!(cache.auth, S3CacheAuth::Sigv4);
    }

    #[test]
    fn s3_cache_auth_is_backward_compatible_without_field() {
        let manifest: Manifest = toml::from_str(
            r#"
[project]
name = "pack"
minecraft = "1.21.1"

[project.loader]
kind = "neo-forge"
version = "latest"

[cache.s3]
enabled = true
bucket = "demo"
path_style = true
upload_enabled = false
"#,
        )
        .expect("manifest should parse");
        let cache = manifest.cache.s3.expect("cache.s3 should exist");
        assert_eq!(cache.auth, S3CacheAuth::Auto);
        assert!(cache.path_style);
        assert!(!cache.upload_enabled);
    }
}
