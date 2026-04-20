use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use mineconda_core::{
    HashAlgorithm, LoaderKind, LockedPackage, Lockfile, Manifest, ModSide, ModSource, ModSpec,
    PackageHash,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

#[derive(Debug, Clone, Copy)]
pub enum ExportFormat {
    CurseforgeZip,
    Mrpack,
    MultiMcZip,
    ModsDescriptionJson,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::CurseforgeZip => "zip",
            Self::Mrpack => "mrpack",
            Self::MultiMcZip => "zip",
            Self::ModsDescriptionJson => "json",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExportRequest {
    pub output: PathBuf,
    pub format: ExportFormat,
    pub project_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFormat {
    Mrpack,
}

impl ImportFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mrpack => "modrinth-mrpack",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSide {
    Client,
    Server,
    Both,
}

#[derive(Debug, Clone)]
pub struct ImportRequest {
    pub input: PathBuf,
    pub side: ImportSide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideScope {
    Common,
    Client,
    Server,
}

#[derive(Debug, Clone)]
pub struct ImportedOverrideFile {
    pub scope: OverrideScope,
    pub relative_path: PathBuf,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ImportResult {
    pub format: ImportFormat,
    pub manifest: Manifest,
    pub lockfile: Lockfile,
    pub overrides: Vec<ImportedOverrideFile>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MrpackIndex {
    format_version: u64,
    game: String,
    version_id: String,
    name: String,
    #[allow(dead_code)]
    summary: Option<String>,
    dependencies: HashMap<String, String>,
    files: Vec<MrpackFile>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MrpackFile {
    path: String,
    hashes: HashMap<String, String>,
    downloads: Vec<String>,
    file_size: u64,
    #[serde(default)]
    env: Option<MrpackFileEnv>,
}

#[derive(Debug, Deserialize)]
struct MrpackFileEnv {
    #[serde(default)]
    client: Option<String>,
    #[serde(default)]
    server: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvSupport {
    Required,
    Optional,
    Unsupported,
}

pub fn export_pack(
    manifest: &Manifest,
    lockfile: &Lockfile,
    request: &ExportRequest,
) -> Result<PathBuf> {
    let output = normalized_output_path(request);
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    match request.format {
        ExportFormat::ModsDescriptionJson => {
            let body = serde_json::to_vec_pretty(&build_mods_description(manifest, lockfile))
                .context("failed to encode mods description")?;
            fs::write(&output, body)
                .with_context(|| format!("failed to write {}", output.display()))?;
        }
        ExportFormat::CurseforgeZip | ExportFormat::Mrpack | ExportFormat::MultiMcZip => {
            let file = File::create(&output)
                .with_context(|| format!("failed to create {}", output.display()))?;
            let mut writer = ZipWriter::new(file);
            let options =
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

            match request.format {
                ExportFormat::CurseforgeZip => {
                    writer.start_file("manifest.json", options)?;
                    writer.write_all(
                        build_curseforge_manifest(manifest, lockfile)
                            .to_string()
                            .as_bytes(),
                    )?;
                }
                ExportFormat::Mrpack => {
                    writer.start_file("modrinth.index.json", options)?;
                    writer.write_all(
                        build_mrpack_index(manifest, lockfile)?
                            .to_string()
                            .as_bytes(),
                    )?;
                    if let Some(root) = request.project_root.as_ref() {
                        write_mrpack_overrides(&mut writer, options, root)?;
                    }
                }
                ExportFormat::MultiMcZip => {
                    writer.start_file("mmc-pack.json", options)?;
                    writer.write_all(
                        build_multimc_manifest(manifest, lockfile)
                            .to_string()
                            .as_bytes(),
                    )?;
                }
                ExportFormat::ModsDescriptionJson => unreachable!(),
            }

            writer.finish()?;
        }
    }
    Ok(output)
}

pub fn detect_pack_format(input: &Path) -> Result<ImportFormat> {
    let file = File::open(input)
        .with_context(|| format!("failed to open modpack archive {}", input.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("failed to parse ZIP archive {}", input.display()))?;

    if archive.by_name("modrinth.index.json").is_ok() {
        return Ok(ImportFormat::Mrpack);
    }

    bail!(
        "unsupported modpack format for {} (currently only Modrinth .mrpack is supported)",
        input.display()
    )
}

pub fn import_pack(request: &ImportRequest) -> Result<ImportResult> {
    let format = detect_pack_format(&request.input)?;
    import_pack_with_format(request, format)
}

pub fn import_pack_with_format(
    request: &ImportRequest,
    format: ImportFormat,
) -> Result<ImportResult> {
    match format {
        ImportFormat::Mrpack => import_mrpack(request),
    }
}

fn import_mrpack(request: &ImportRequest) -> Result<ImportResult> {
    let file = File::open(&request.input)
        .with_context(|| format!("failed to open modpack archive {}", request.input.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("failed to parse ZIP archive {}", request.input.display()))?;

    let mut index_raw = String::new();
    archive
        .by_name("modrinth.index.json")
        .context("modrinth.index.json not found in archive")?
        .read_to_string(&mut index_raw)
        .context("failed to read modrinth.index.json")?;
    let index: MrpackIndex =
        serde_json::from_str(&index_raw).context("failed to parse modrinth.index.json")?;

    if index.format_version != 1 {
        bail!(
            "unsupported mrpack formatVersion {} (expected 1)",
            index.format_version
        );
    }
    if index.game != "minecraft" {
        bail!(
            "unsupported mrpack game {} (expected minecraft)",
            index.game
        );
    }
    if index.version_id.trim().is_empty() {
        bail!("mrpack versionId must not be empty");
    }

    let minecraft = index
        .dependencies
        .get("minecraft")
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("mrpack dependencies.minecraft is required")?
        .to_string();

    let (loader_kind, loader_version) = extract_loader_dependency(&index.dependencies)?;
    let mut manifest = Manifest::new(index.name, minecraft, loader_kind, loader_version);

    let mut mods = Vec::new();
    let mut packages = Vec::new();
    let mut seen = HashSet::new();

    for file in &index.files {
        let normalized_path = validate_relative_path(file.path.as_str())
            .with_context(|| format!("invalid mrpack file path {}", file.path))?;
        let normalized_path_string = path_to_forward_slashes(&normalized_path);
        let file_name = normalized_path
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| !value.trim().is_empty())
            .with_context(|| format!("mrpack file path {} has no file name", file.path))?
            .to_string();

        if file.downloads.is_empty() {
            bail!("mrpack entry {} has no downloads", file.path);
        }

        for url in &file.downloads {
            validate_mrpack_download_url(url)
                .with_context(|| format!("invalid download URL in mrpack entry {}", file.path))?;
        }

        let normalized_hashes = normalize_hash_map(&file.hashes);
        let sha1 = required_hash(&normalized_hashes, "sha1", file.path.as_str())?;
        let sha512 = required_hash(&normalized_hashes, "sha512", file.path.as_str())?;
        let hashes = package_hashes_from_index(&normalized_hashes);
        let sha256 = normalized_hashes
            .get("sha256")
            .map(String::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("pending")
            .to_string();

        let side = side_from_mrpack_env(file.env.as_ref())
            .with_context(|| format!("invalid env in mrpack entry {}", file.path))?;

        let first_download = file
            .downloads
            .first()
            .expect("downloads is checked as non-empty")
            .clone();

        let (id, source, version, source_ref) = if let Some((project_id, version_id)) =
            parse_modrinth_download_coordinates(&first_download)
        {
            (
                project_id.clone(),
                ModSource::Modrinth,
                version_id.clone(),
                Some(format!("project={project_id};version={version_id}")),
            )
        } else {
            (
                derive_mod_id_from_file_name(&file_name),
                ModSource::Url,
                first_download.clone(),
                None,
            )
        };

        let key = format!("{}@{}#{}", id, source.as_str(), normalized_path_string);
        if !seen.insert(key.clone()) {
            bail!("mrpack contains duplicate mod identity {key}");
        }

        let mut lock_hashes = hashes;
        if lock_hashes
            .iter()
            .all(|entry| entry.algorithm != HashAlgorithm::Sha1)
        {
            lock_hashes.push(PackageHash {
                algorithm: HashAlgorithm::Sha1,
                value: sha1.to_string(),
            });
        }
        if lock_hashes
            .iter()
            .all(|entry| entry.algorithm != HashAlgorithm::Sha512)
        {
            lock_hashes.push(PackageHash {
                algorithm: HashAlgorithm::Sha512,
                value: sha512.to_string(),
            });
        }

        mods.push(ModSpec::new(id.clone(), source, version.clone(), side));
        if let Some(spec) = mods.last_mut() {
            spec.install_path = Some(normalized_path_string.clone());
        }
        packages.push(LockedPackage {
            id,
            source,
            version,
            side,
            file_name,
            install_path: Some(normalized_path_string),
            file_size: Some(file.file_size),
            sha256,
            download_url: first_download,
            hashes: lock_hashes,
            source_ref,
        });
    }

    manifest.mods = mods;
    let lockfile = Lockfile::from_packages(&manifest, packages);
    let overrides = collect_mrpack_overrides(&mut archive, request.side)?;

    Ok(ImportResult {
        format: ImportFormat::Mrpack,
        manifest,
        lockfile,
        overrides,
    })
}

fn normalized_output_path(request: &ExportRequest) -> PathBuf {
    if request.output.extension().is_some() {
        request.output.clone()
    } else {
        request.output.with_extension(request.format.extension())
    }
}

fn build_curseforge_manifest(manifest: &Manifest, lockfile: &Lockfile) -> serde_json::Value {
    let mut skipped = Vec::new();
    let files: Vec<_> = lockfile
        .packages
        .iter()
        .filter_map(|pkg| {
            if pkg.source != ModSource::Curseforge {
                return None;
            }

            let project_id = parse_u64_or_source_ref(&pkg.id, pkg.source_ref.as_deref(), "mod");
            let file_id = parse_u64_or_source_ref(&pkg.version, pkg.source_ref.as_deref(), "file");

            let (Some(project_id), Some(file_id)) = (project_id, file_id) else {
                skipped.push(format!(
                    "{}@{} (missing numeric project/file id)",
                    pkg.id, pkg.version
                ));
                return None;
            };

            Some(json!({
                "projectID": project_id,
                "fileID": file_id,
                "required": true
            }))
        })
        .collect();

    let loader = format!(
        "{}-{}",
        manifest.project.loader.kind.as_str(),
        manifest.project.loader.version.clone()
    );

    let mut root = json!({
        "manifestType": "minecraftModpack",
        "manifestVersion": 1,
        "name": manifest.project.name.clone(),
        "version": "0.1.0",
        "author": "mineconda",
        "minecraft": {
            "version": manifest.project.minecraft.clone(),
            "modLoaders": [
                {
                    "id": loader,
                    "primary": true
                }
            ]
        },
        "files": files,
        "overrides": "overrides"
    });

    if !skipped.is_empty()
        && let Some(object) = root.as_object_mut()
    {
        object.insert(
            "x-mineconda".to_string(),
            json!({
                "skipped_non_curseforge_entries": skipped,
            }),
        );
    }

    root
}

fn build_mrpack_index(manifest: &Manifest, lockfile: &Lockfile) -> Result<serde_json::Value> {
    let files: Vec<_> = lockfile
        .packages
        .iter()
        .map(|pkg| {
            validate_export_file_name(pkg.file_name.as_str()).with_context(|| {
                format!(
                    "invalid file_name {} for package {}@{}",
                    pkg.file_name, pkg.id, pkg.version
                )
            })?;

            validate_mrpack_download_url(pkg.download_url.as_str()).with_context(|| {
                format!(
                    "invalid download URL {} for package {}@{}",
                    pkg.download_url, pkg.id, pkg.version
                )
            })?;

            let file_size = pkg.file_size.with_context(|| {
                format!(
                    "mrpack export requires file_size for package {}@{}",
                    pkg.id, pkg.version
                )
            })?;

            let hashes = mrpack_hashes_for_package(pkg).with_context(|| {
                format!(
                    "mrpack export requires sha1 and sha512 hashes for package {}@{}",
                    pkg.id, pkg.version
                )
            })?;

            let path = package_install_path(pkg).with_context(|| {
                format!(
                    "invalid install path for package {}@{}",
                    pkg.id, pkg.version
                )
            })?;

            Ok(json!({
                "path": path,
                "hashes": hashes,
                "downloads": [pkg.download_url.clone()],
                "fileSize": file_size,
                "env": mrpack_env_for_side(pkg.side),
            }))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut dependencies = Map::new();
    dependencies.insert(
        "minecraft".to_string(),
        json!(manifest.project.minecraft.clone()),
    );
    dependencies.insert(
        manifest.project.loader.kind.as_str().to_string(),
        json!(manifest.project.loader.version.clone()),
    );

    Ok(json!({
        "formatVersion": 1,
        "game": "minecraft",
        "versionId": "0.1.0",
        "name": manifest.project.name.clone(),
        "summary": "Generated by mineconda",
        "dependencies": Value::Object(dependencies),
        "files": files
    }))
}

fn build_multimc_manifest(manifest: &Manifest, lockfile: &Lockfile) -> serde_json::Value {
    let components = vec![
        json!({
            "uid": "net.minecraft",
            "version": manifest.project.minecraft.clone(),
            "important": true
        }),
        json!({
            "uid": manifest.project.loader.kind.as_str(),
            "version": manifest.project.loader.version.clone()
        }),
    ];

    json!({
        "formatVersion": 1,
        "name": manifest.project.name.clone(),
        "components": components,
        "mods": lockfile.packages.iter().map(|pkg| {
            json!({
                "id": pkg.id.clone(),
                "version": pkg.version.clone(),
                "source": pkg.source.as_str()
            })
        }).collect::<Vec<_>>()
    })
}

fn build_mods_description(manifest: &Manifest, lockfile: &Lockfile) -> serde_json::Value {
    let declared: Vec<_> = manifest
        .mods
        .iter()
        .map(|entry| {
            json!({
                "id": entry.id,
                "source": entry.source.as_str(),
                "requested": entry.version,
                "side": entry.side.as_str()
            })
        })
        .collect();

    let resolved: Vec<_> = lockfile
        .packages
        .iter()
        .map(|pkg| {
            json!({
                "id": pkg.id,
                "source": pkg.source.as_str(),
                "version": pkg.version,
                "side": pkg.side.as_str(),
                "file": pkg.file_name,
                "file_size": pkg.file_size,
                "download_url": pkg.download_url,
                "source_ref": pkg.source_ref,
                "hashes": pkg.hashes.iter().map(|h| json!({
                    "algorithm": h.algorithm.as_str(),
                    "value": h.value
                })).collect::<Vec<_>>()
            })
        })
        .collect();

    json!({
        "project": {
            "name": manifest.project.name,
            "minecraft": manifest.project.minecraft,
            "loader": {
                "kind": manifest.project.loader.kind.as_str(),
                "version": manifest.project.loader.version
            }
        },
        "declared_mods": declared,
        "resolved_mods": resolved
    })
}

fn parse_u64_or_source_ref(raw: &str, source_ref: Option<&str>, key: &str) -> Option<u64> {
    raw.parse::<u64>().ok().or_else(|| {
        source_ref
            .and_then(|value| parse_source_ref_field(value, key))
            .and_then(|value| value.parse::<u64>().ok())
    })
}

fn parse_source_ref_field(source_ref: &str, key: &str) -> Option<String> {
    source_ref
        .split(';')
        .filter_map(|part| part.split_once('='))
        .find_map(|(k, v)| (k.trim() == key).then(|| v.trim().to_string()))
}

fn mrpack_hashes_for_package(pkg: &LockedPackage) -> Result<Value> {
    let mut map = Map::new();
    if let Some(value) = pkg.hash(HashAlgorithm::Sha1)
        && !value.trim().is_empty()
        && value != "pending"
    {
        map.insert("sha1".to_string(), Value::String(value.to_string()));
    }
    if let Some(value) = pkg.hash(HashAlgorithm::Sha512)
        && !value.trim().is_empty()
        && value != "pending"
    {
        map.insert("sha512".to_string(), Value::String(value.to_string()));
    }

    if !map.contains_key("sha1") {
        bail!("missing sha1 hash");
    }
    if !map.contains_key("sha512") {
        bail!("missing sha512 hash");
    }

    Ok(Value::Object(map))
}

fn package_hashes_from_index(raw: &HashMap<String, String>) -> Vec<PackageHash> {
    let mut hashes = Vec::new();
    for algorithm in [
        HashAlgorithm::Sha1,
        HashAlgorithm::Sha256,
        HashAlgorithm::Sha512,
        HashAlgorithm::Md5,
    ] {
        let key = algorithm.as_str();
        if let Some(value) = raw
            .get(key)
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            hashes.push(PackageHash {
                algorithm,
                value: value.to_string(),
            });
        }
    }
    hashes
}

fn extract_loader_dependency(
    dependencies: &HashMap<String, String>,
) -> Result<(LoaderKind, String)> {
    let mut found = Vec::new();
    for (key, kind) in [
        ("fabric-loader", LoaderKind::Fabric),
        ("forge", LoaderKind::Forge),
        ("neoforge", LoaderKind::NeoForge),
        ("quilt-loader", LoaderKind::Quilt),
    ] {
        if let Some(version) = dependencies
            .get(key)
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            found.push((kind, version.to_string()));
        }
    }

    if found.len() != 1 {
        bail!(
            "mrpack dependencies must contain exactly one loader key (fabric-loader|forge|neoforge|quilt-loader)"
        );
    }

    Ok(found.remove(0))
}

fn collect_mrpack_overrides(
    archive: &mut ZipArchive<File>,
    side: ImportSide,
) -> Result<Vec<ImportedOverrideFile>> {
    let mut merged = BTreeMap::new();
    for (prefix, scope) in override_layers(side) {
        for idx in 0..archive.len() {
            let mut entry = archive
                .by_index(idx)
                .with_context(|| format!("failed to access zip entry #{idx}"))?;
            if entry.is_dir() {
                continue;
            }
            let Some(relative) = entry.name().strip_prefix(prefix) else {
                continue;
            };
            if relative.trim().is_empty() {
                continue;
            }

            let relative_path = validate_relative_path(relative).with_context(|| {
                format!(
                    "invalid path {} under {}",
                    relative,
                    prefix.trim_end_matches('/')
                )
            })?;

            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .with_context(|| format!("failed to read zip entry {}", entry.name()))?;

            merged.insert(
                relative_path.clone(),
                ImportedOverrideFile {
                    scope,
                    relative_path,
                    bytes,
                },
            );
        }
    }

    Ok(merged.into_values().collect())
}

fn override_layers(side: ImportSide) -> Vec<(&'static str, OverrideScope)> {
    let mut layers = vec![("overrides/", OverrideScope::Common)];
    match side {
        ImportSide::Client => layers.push(("client-overrides/", OverrideScope::Client)),
        ImportSide::Server => layers.push(("server-overrides/", OverrideScope::Server)),
        ImportSide::Both => {
            layers.push(("client-overrides/", OverrideScope::Client));
            layers.push(("server-overrides/", OverrideScope::Server));
        }
    }
    layers
}

fn validate_mrpack_download_url(url: &str) -> Result<()> {
    if !url.starts_with("https://") {
        bail!("mrpack download URL must use https");
    }
    if url.chars().any(char::is_whitespace) {
        bail!("mrpack download URL contains whitespace");
    }
    Ok(())
}

fn validate_export_file_name(file_name: &str) -> Result<()> {
    if file_name.trim().is_empty() {
        bail!("empty file name is not allowed");
    }
    if file_name.contains('/') || file_name.contains('\\') {
        bail!("file name must not contain path separators");
    }
    if file_name == "." || file_name == ".." {
        bail!("invalid file name");
    }
    Ok(())
}

fn validate_relative_path(raw: &str) -> Result<PathBuf> {
    if raw.trim().is_empty() {
        bail!("path must not be empty");
    }
    if raw.starts_with('/') || raw.starts_with('\\') {
        bail!("absolute path is not allowed");
    }
    if raw.contains('\\') {
        bail!("path must use forward slashes");
    }

    let path = Path::new(raw);
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => normalized.push(value),
            Component::CurDir => {}
            Component::ParentDir => bail!("parent traversal is not allowed"),
            Component::Prefix(_) | Component::RootDir => bail!("absolute path is not allowed"),
        }
    }

    if normalized.as_os_str().is_empty() {
        bail!("path must not be empty");
    }
    Ok(normalized)
}

fn path_to_forward_slashes(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn package_install_path(pkg: &LockedPackage) -> Result<String> {
    let raw = pkg.install_path_or_default();
    let normalized = validate_relative_path(raw.as_str())?;
    Ok(path_to_forward_slashes(&normalized))
}

fn write_mrpack_overrides(
    writer: &mut ZipWriter<File>,
    options: SimpleFileOptions,
    project_root: &Path,
) -> Result<()> {
    let mut entries = Vec::new();
    collect_override_entries(project_root, "overrides", &mut entries)?;
    collect_override_entries(project_root, "client-overrides", &mut entries)?;
    collect_override_entries(project_root, "server-overrides", &mut entries)?;
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    for (zip_path, bytes) in entries {
        writer
            .start_file(&zip_path, options)
            .with_context(|| format!("failed to start zip entry {zip_path}"))?;
        writer
            .write_all(&bytes)
            .with_context(|| format!("failed to write zip entry {zip_path}"))?;
    }
    Ok(())
}

fn collect_override_entries(
    project_root: &Path,
    layer: &str,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<()> {
    let root = project_root.join(layer);
    if !root.exists() {
        return Ok(());
    }

    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in
            fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                stack.push(path);
                continue;
            }
            if !entry.file_type()?.is_file() {
                continue;
            }

            let relative = path.strip_prefix(project_root).with_context(|| {
                format!("failed to compute relative path for {}", path.display())
            })?;
            let normalized = validate_relative_path(path_to_forward_slashes(relative).as_str())?;
            let zip_path = path_to_forward_slashes(&normalized);
            let bytes =
                fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
            out.push((zip_path, bytes));
        }
    }

    Ok(())
}

fn normalize_hash_map(raw: &HashMap<String, String>) -> HashMap<String, String> {
    let mut normalized = HashMap::new();
    for (key, value) in raw {
        let key = key.to_ascii_lowercase();
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        normalized.insert(key, value.to_string());
    }
    normalized
}

fn required_hash<'a>(
    hashes: &'a HashMap<String, String>,
    key: &str,
    path: &str,
) -> Result<&'a str> {
    hashes
        .get(key)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("mrpack entry {path} is missing required hash {key}"))
}

fn side_from_mrpack_env(env: Option<&MrpackFileEnv>) -> Result<ModSide> {
    let Some(env) = env else {
        return Ok(ModSide::Both);
    };

    let client = env_support_from_str(env.client.as_deref())?;
    let server = env_support_from_str(env.server.as_deref())?;

    if client == EnvSupport::Unsupported && server == EnvSupport::Unsupported {
        bail!("both client and server are unsupported");
    }

    if client == EnvSupport::Unsupported {
        return Ok(ModSide::Server);
    }
    if server == EnvSupport::Unsupported {
        return Ok(ModSide::Client);
    }

    Ok(ModSide::Both)
}

fn env_support_from_str(raw: Option<&str>) -> Result<EnvSupport> {
    match raw.unwrap_or("optional") {
        "required" => Ok(EnvSupport::Required),
        "optional" => Ok(EnvSupport::Optional),
        "unsupported" => Ok(EnvSupport::Unsupported),
        other => bail!("invalid env support value {other}"),
    }
}

fn parse_modrinth_download_coordinates(url: &str) -> Option<(String, String)> {
    let clean = url.split('?').next().unwrap_or(url);
    let clean = clean.split('#').next().unwrap_or(clean);
    let parts: Vec<&str> = clean
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    for window in parts.windows(4) {
        if window[0] == "data" && window[2] == "versions" {
            let project = window[1].trim();
            let version = window[3].trim();
            if !project.is_empty() && !version.is_empty() {
                return Some((project.to_string(), version.to_string()));
            }
        }
    }
    None
}

fn derive_mod_id_from_file_name(file_name: &str) -> String {
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("imported-mod");

    let mut out = String::with_capacity(stem.len());
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "imported-mod".to_string()
    } else {
        out
    }
}

fn mrpack_env_for_side(side: ModSide) -> Value {
    match side {
        ModSide::Both => json!({
            "client": "required",
            "server": "required"
        }),
        ModSide::Client => json!({
            "client": "required",
            "server": "unsupported"
        }),
        ModSide::Server => json!({
            "client": "unsupported",
            "server": "required"
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mineconda_core::{
        LoaderSpec, LockMetadata, ProjectSection, RuntimeProfile, ServerProfile, SourceRegistry,
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn mrpack_file_keeps_size_hash_and_side_env() {
        let manifest = sample_manifest();
        let lock = Lockfile {
            metadata: sample_metadata(),
            packages: vec![LockedPackage {
                id: "abc".to_string(),
                source: ModSource::Modrinth,
                version: "v1".to_string(),
                side: ModSide::Client,
                file_name: "mod.jar".to_string(),
                install_path: None,
                file_size: Some(1234),
                sha256: "deadbeef".to_string(),
                download_url: "https://cdn.example/mod.jar".to_string(),
                hashes: vec![
                    PackageHash {
                        algorithm: HashAlgorithm::Sha1,
                        value: "sha1v".to_string(),
                    },
                    PackageHash {
                        algorithm: HashAlgorithm::Sha512,
                        value: "sha512v".to_string(),
                    },
                ],
                source_ref: Some("project=abc;version=v1".to_string()),
            }],
        };

        let index = build_mrpack_index(&manifest, &lock).expect("mrpack export should succeed");
        let files = index["files"]
            .as_array()
            .expect("mrpack index should contain files array");
        let file = files.first().expect("mrpack files should not be empty");
        assert_eq!(file["fileSize"], json!(1234));
        assert_eq!(file["env"]["client"], json!("required"));
        assert_eq!(file["env"]["server"], json!("unsupported"));
        assert_eq!(file["hashes"]["sha1"], json!("sha1v"));
        assert_eq!(file["hashes"]["sha512"], json!("sha512v"));
    }

    #[test]
    fn mrpack_index_rejects_entries_with_unsupported_download_urls() {
        let manifest = sample_manifest();
        let lock = Lockfile {
            metadata: sample_metadata(),
            packages: vec![LockedPackage {
                id: "local".to_string(),
                source: ModSource::Local,
                version: "1.0.0".to_string(),
                side: ModSide::Both,
                file_name: "local.jar".to_string(),
                install_path: None,
                file_size: Some(10),
                sha256: "def".to_string(),
                download_url: "vendor/local.jar".to_string(),
                hashes: vec![
                    PackageHash {
                        algorithm: HashAlgorithm::Sha1,
                        value: "sha1v".to_string(),
                    },
                    PackageHash {
                        algorithm: HashAlgorithm::Sha512,
                        value: "sha512v".to_string(),
                    },
                ],
                source_ref: None,
            }],
        };

        let err = build_mrpack_index(&manifest, &lock).expect_err("expected strict error");
        assert!(format!("{err:#}").contains("https"));
    }

    #[test]
    fn mrpack_index_rejects_entries_missing_required_hashes() {
        let manifest = sample_manifest();
        let lock = Lockfile {
            metadata: sample_metadata(),
            packages: vec![LockedPackage {
                id: "remote".to_string(),
                source: ModSource::Modrinth,
                version: "1.0.0".to_string(),
                side: ModSide::Both,
                file_name: "remote.jar".to_string(),
                install_path: None,
                file_size: Some(10),
                sha256: "abc".to_string(),
                download_url: "https://cdn.example/remote.jar".to_string(),
                hashes: vec![PackageHash {
                    algorithm: HashAlgorithm::Sha1,
                    value: "sha1v".to_string(),
                }],
                source_ref: None,
            }],
        };

        let err = build_mrpack_index(&manifest, &lock).expect_err("expected strict error");
        assert!(format!("{err:#}").contains("sha512"));
    }

    #[test]
    fn detect_pack_format_recognizes_mrpack() {
        let archive = write_test_mrpack(
            "detect",
            r#"{
                "formatVersion": 1,
                "game": "minecraft",
                "versionId": "1.0.0",
                "name": "Detect",
                "summary": "test",
                "dependencies": {"minecraft": "1.21.1", "neoforge": "21.1.1"},
                "files": []
            }"#,
            &[],
        );
        let format = detect_pack_format(&archive).expect("format detection should succeed");
        assert_eq!(format, ImportFormat::Mrpack);
    }

    #[test]
    fn import_pack_reads_manifest_lock_and_overrides() {
        let index = r#"{
            "formatVersion": 1,
            "game": "minecraft",
            "versionId": "1.0.0",
            "name": "ImportedPack",
            "summary": "test",
            "dependencies": {
                "minecraft": "1.21.1",
                "neoforge": "21.1.227"
            },
            "files": [
                {
                    "path": "mods/ferritecore.jar",
                    "hashes": {
                        "sha1": "111",
                        "sha512": "222",
                        "sha256": "333"
                    },
                    "downloads": [
                        "https://cdn.modrinth.com/data/uXXizFIs/versions/x7kQWVju/ferritecore.jar"
                    ],
                    "fileSize": 123,
                    "env": {
                        "client": "required",
                        "server": "unsupported"
                    }
                }
            ]
        }"#;

        let archive = write_test_mrpack(
            "import",
            index,
            &[
                ("overrides/config/common.toml", b"common=true\n"),
                ("client-overrides/config/client.toml", b"client=true\n"),
                ("server-overrides/config/server.toml", b"server=true\n"),
            ],
        );

        let output = import_pack(&ImportRequest {
            input: archive,
            side: ImportSide::Client,
        })
        .expect("import should succeed");

        assert_eq!(output.format, ImportFormat::Mrpack);
        assert_eq!(output.manifest.project.name, "ImportedPack");
        assert_eq!(output.manifest.project.minecraft, "1.21.1");
        assert_eq!(output.manifest.project.loader.kind, LoaderKind::NeoForge);
        assert_eq!(output.manifest.project.loader.version, "21.1.227");
        assert_eq!(output.manifest.mods.len(), 1);
        assert_eq!(output.manifest.mods[0].source, ModSource::Modrinth);
        assert_eq!(output.manifest.mods[0].id, "uXXizFIs");
        assert_eq!(output.manifest.mods[0].version, "x7kQWVju");
        assert_eq!(
            output.manifest.mods[0].install_path.as_deref(),
            Some("mods/ferritecore.jar")
        );
        assert_eq!(output.manifest.mods[0].side, ModSide::Client);
        assert_eq!(output.lockfile.packages.len(), 1);
        assert_eq!(
            output.lockfile.packages[0].install_path.as_deref(),
            Some("mods/ferritecore.jar")
        );

        let override_paths: Vec<String> = output
            .overrides
            .iter()
            .map(|entry| entry.relative_path.display().to_string())
            .collect();
        assert!(override_paths.contains(&"config/common.toml".to_string()));
        assert!(override_paths.contains(&"config/client.toml".to_string()));
        assert!(!override_paths.contains(&"config/server.toml".to_string()));
    }

    #[test]
    fn import_pack_rejects_missing_required_hash() {
        let archive = write_test_mrpack(
            "missing-hash",
            r#"{
                "formatVersion": 1,
                "game": "minecraft",
                "versionId": "1.0.0",
                "name": "Invalid",
                "summary": "test",
                "dependencies": {"minecraft": "1.21.1", "neoforge": "21.1.1"},
                "files": [
                    {
                        "path": "mods/a.jar",
                        "hashes": {"sha1": "111"},
                        "downloads": ["https://example.com/a.jar"],
                        "fileSize": 1
                    }
                ]
            }"#,
            &[],
        );

        let err = import_pack(&ImportRequest {
            input: archive,
            side: ImportSide::Client,
        })
        .expect_err("import should fail");

        assert!(format!("{err:#}").contains("sha512"));
    }

    #[test]
    fn mrpack_export_uses_package_install_path() {
        let manifest = sample_manifest();
        let lock = Lockfile {
            metadata: sample_metadata(),
            packages: vec![LockedPackage {
                id: "custom".to_string(),
                source: ModSource::Url,
                version: "https://example.com/custom.toml".to_string(),
                side: ModSide::Both,
                file_name: "custom.toml".to_string(),
                install_path: Some("config/imported/custom.toml".to_string()),
                file_size: Some(15),
                sha256: "deadbeef".to_string(),
                download_url: "https://example.com/custom.toml".to_string(),
                hashes: vec![
                    PackageHash {
                        algorithm: HashAlgorithm::Sha1,
                        value: "sha1v".to_string(),
                    },
                    PackageHash {
                        algorithm: HashAlgorithm::Sha512,
                        value: "sha512v".to_string(),
                    },
                ],
                source_ref: None,
            }],
        };

        let index = build_mrpack_index(&manifest, &lock).expect("mrpack export should succeed");
        let files = index["files"]
            .as_array()
            .expect("mrpack index should contain files array");
        let file = files.first().expect("mrpack files should not be empty");
        assert_eq!(file["path"], json!("config/imported/custom.toml"));
    }

    #[test]
    fn mrpack_export_writes_overrides_layers_when_present() {
        let manifest = sample_manifest();
        let lock = Lockfile {
            metadata: sample_metadata(),
            packages: vec![LockedPackage {
                id: "abc".to_string(),
                source: ModSource::Modrinth,
                version: "v1".to_string(),
                side: ModSide::Both,
                file_name: "mod.jar".to_string(),
                install_path: None,
                file_size: Some(1),
                sha256: "pending".to_string(),
                download_url: "https://example.com/mod.jar".to_string(),
                hashes: vec![
                    PackageHash {
                        algorithm: HashAlgorithm::Sha1,
                        value: "sha1v".to_string(),
                    },
                    PackageHash {
                        algorithm: HashAlgorithm::Sha512,
                        value: "sha512v".to_string(),
                    },
                ],
                source_ref: None,
            }],
        };

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("mineconda-export-overrides-{unique}"));
        fs::create_dir_all(root.join("overrides/config")).expect("failed to create overrides dir");
        fs::create_dir_all(root.join("client-overrides/config"))
            .expect("failed to create client-overrides dir");
        fs::write(root.join("overrides/config/common.toml"), b"common=true\n")
            .expect("failed to write overrides file");
        fs::write(
            root.join("client-overrides/config/client.toml"),
            b"client=true\n",
        )
        .expect("failed to write client-overrides file");

        let output = root.join("dist/pack");
        let exported = export_pack(
            &manifest,
            &lock,
            &ExportRequest {
                output,
                format: ExportFormat::Mrpack,
                project_root: Some(root.clone()),
            },
        )
        .expect("export should succeed");

        let file = File::open(&exported).expect("failed to open exported archive");
        let mut archive = ZipArchive::new(file).expect("failed to parse exported archive");
        archive
            .by_name("overrides/config/common.toml")
            .expect("missing common override");
        archive
            .by_name("client-overrides/config/client.toml")
            .expect("missing client override");
    }

    #[test]
    fn curseforge_manifest_only_contains_numeric_entries() {
        let manifest = sample_manifest();
        let lock = Lockfile {
            metadata: sample_metadata(),
            packages: vec![
                LockedPackage {
                    id: "325492".to_string(),
                    source: ModSource::Curseforge,
                    version: "5940240".to_string(),
                    side: ModSide::Both,
                    file_name: "valid.jar".to_string(),
                    install_path: None,
                    file_size: None,
                    sha256: "pending".to_string(),
                    download_url: "https://example.com/valid.jar".to_string(),
                    hashes: Vec::new(),
                    source_ref: Some("mod=325492;file=5940240".to_string()),
                },
                LockedPackage {
                    id: "invalid".to_string(),
                    source: ModSource::Curseforge,
                    version: "invalid".to_string(),
                    side: ModSide::Both,
                    file_name: "invalid.jar".to_string(),
                    install_path: None,
                    file_size: None,
                    sha256: "pending".to_string(),
                    download_url: "https://example.com/invalid.jar".to_string(),
                    hashes: Vec::new(),
                    source_ref: Some("mod=348521;file=6150677".to_string()),
                },
                LockedPackage {
                    id: "modrinth".to_string(),
                    source: ModSource::Modrinth,
                    version: "1.0.0".to_string(),
                    side: ModSide::Both,
                    file_name: "other.jar".to_string(),
                    install_path: None,
                    file_size: None,
                    sha256: "pending".to_string(),
                    download_url: "https://example.com/other.jar".to_string(),
                    hashes: Vec::new(),
                    source_ref: None,
                },
            ],
        };

        let manifest_json = build_curseforge_manifest(&manifest, &lock);
        let files = manifest_json["files"]
            .as_array()
            .expect("curseforge manifest should contain files array");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0]["projectID"], json!(325492));
        assert_eq!(files[0]["fileID"], json!(5940240));
        assert_eq!(files[1]["projectID"], json!(348521));
        assert_eq!(files[1]["fileID"], json!(6150677));
    }

    fn sample_manifest() -> Manifest {
        Manifest {
            project: ProjectSection {
                name: "pack".to_string(),
                minecraft: "1.21.1".to_string(),
                loader: LoaderSpec {
                    kind: LoaderKind::NeoForge,
                    version: "latest".to_string(),
                },
            },
            mods: vec![ModSpec {
                id: "abc".to_string(),
                source: ModSource::Modrinth,
                version: "latest".to_string(),
                install_path: None,
                side: ModSide::Both,
            }],
            sources: SourceRegistry::default(),
            cache: Default::default(),
            server: ServerProfile::default(),
            runtime: RuntimeProfile::default(),
        }
    }

    fn sample_metadata() -> LockMetadata {
        LockMetadata {
            generated_by: "mineconda/0.1.0".to_string(),
            generated_at_unix: 0,
            minecraft: "1.21.1".to_string(),
            loader: LoaderSpec {
                kind: LoaderKind::NeoForge,
                version: "latest".to_string(),
            },
        }
    }

    fn write_test_mrpack(tag: &str, index: &str, extras: &[(&str, &[u8])]) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mineconda-{tag}-{unique}.mrpack"));
        let file = File::create(&path).expect("failed to create temp mrpack");
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        zip.start_file("modrinth.index.json", options)
            .expect("failed to create index entry");
        zip.write_all(index.as_bytes())
            .expect("failed to write index");

        for (name, bytes) in extras {
            zip.start_file(name, options)
                .expect("failed to create extra entry");
            zip.write_all(bytes).expect("failed to write extra entry");
        }

        zip.finish().expect("failed to finish zip");
        path
    }
}
