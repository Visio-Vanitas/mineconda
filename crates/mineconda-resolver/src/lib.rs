use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use mineconda_core::{
    DEFAULT_GROUP_NAME, HashAlgorithm, LoaderKind, LockedDependency, LockedDependencyKind,
    LockedPackage, Lockfile, Manifest, ModSide, ModSource, PackageHash, S3SourceConfig,
    http_user_agent,
};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

const MODRINTH_SEARCH_API: &str = "https://api.modrinth.com/v2/search";
const MODRINTH_PROJECT_VERSION_API: &str = "https://api.modrinth.com/v2/project";
const CURSEFORGE_SEARCH_API: &str = "https://api.curseforge.com/v1/mods/search";
const CURSEFORGE_FILES_API: &str = "https://api.curseforge.com/v1/mods";
const MCMOD_SEARCH_API: &str = "https://search.mcmod.cn/s";
const FABRIC_LOADER_META_API: &str = "https://meta.fabricmc.net/v2/versions/loader";
const QUILT_LOADER_META_API: &str = "https://meta.quiltmc.org/v3/versions/loader";
const NEOFORGE_MAVEN_METADATA_API: &str =
    "https://maven.neoforged.net/releases/net/neoforged/neoforge/maven-metadata.xml";
const FORGE_MAVEN_METADATA_API: &str =
    "https://maven.minecraftforge.net/net/minecraftforge/forge/maven-metadata.xml";
const MINECRAFT_GAME_ID: usize = 432;
const SEARCH_CACHE_TTL_SECS: u64 = 30 * 60;
const SEARCH_CACHE_SCHEMA_VERSION: u32 = 2;
const HTTP_RETRY_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Default)]
pub struct ResolveRequest {
    pub upgrade: bool,
    pub groups: BTreeSet<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ResolutionPlan {
    pub install: Vec<LockedPackage>,
    pub remove: Vec<String>,
    pub unchanged: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ResolveOutput {
    pub lockfile: Lockfile,
    pub plan: ResolutionPlan,
}

#[derive(Debug, Clone)]
struct ResolutionRequirement {
    source: ModSource,
    id: String,
    constraint: VersionConstraint,
    install_path: Option<String>,
    side: ModSide,
    groups: BTreeSet<String>,
    requested_by: String,
}

#[derive(Debug, Clone)]
struct ResolvedEntry {
    package: LockedPackage,
    resolved_version_id: Option<String>,
    resolved_name: Option<String>,
    dependencies: Vec<DependencyEdge>,
}

#[derive(Debug, Clone)]
struct DependencyEdge {
    source: ModSource,
    id: String,
    constraint: VersionConstraint,
    kind: DependencyKind,
    groups: BTreeSet<String>,
    requested_by: String,
}

fn manifest_request_chain(group: &str, source: ModSource, id: &str) -> String {
    format!("manifest[{group}] -> {id} [{}]", source.as_str())
}

fn extend_request_chain(chain: &str, source: ModSource, id: &str, version: &str) -> String {
    format!("{chain} -> {id} [{}]@{version}", source.as_str())
}

fn locked_dependency_from_edge(edge: &DependencyEdge) -> LockedDependency {
    LockedDependency {
        source: edge.source,
        id: edge.id.clone(),
        kind: match edge.kind {
            DependencyKind::Required => LockedDependencyKind::Required,
            DependencyKind::Incompatible => LockedDependencyKind::Incompatible,
        },
        constraint: if edge.constraint.is_any() {
            None
        } else {
            Some(edge.constraint.describe())
        },
    }
}

fn package_groups(groups: &BTreeSet<String>) -> Vec<String> {
    let mut values: Vec<String> = groups.iter().cloned().collect();
    values.sort_by(
        |left, right| match (left == DEFAULT_GROUP_NAME, right == DEFAULT_GROUP_NAME) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => left.cmp(right),
        },
    );
    values
}

fn root_group_set(group: &str) -> BTreeSet<String> {
    BTreeSet::from([group.to_string()])
}

fn active_groups(request: &ResolveRequest) -> BTreeSet<String> {
    if request.groups.is_empty() {
        return BTreeSet::from([DEFAULT_GROUP_NAME.to_string()]);
    }

    let mut groups = request.groups.clone();
    groups.insert(DEFAULT_GROUP_NAME.to_string());
    groups
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DependencyKind {
    Required,
    Incompatible,
}

#[derive(Debug, Clone, Default)]
struct VersionConstraint {
    exact: Option<String>,
    ranges: Vec<SemverRange>,
}

#[derive(Debug, Clone)]
struct SemverRange {
    raw: String,
    req: VersionReq,
}

impl VersionConstraint {
    fn any() -> Self {
        Self::default()
    }

    fn exact(value: impl Into<String>) -> Self {
        Self {
            exact: Some(value.into()),
            ranges: Vec::new(),
        }
    }

    fn from_raw(raw: &str) -> Self {
        if raw.eq_ignore_ascii_case("latest") {
            return Self::any();
        }

        if looks_like_semver_range(raw)
            && let Ok(req) = VersionReq::parse(raw)
        {
            return Self {
                exact: None,
                ranges: vec![SemverRange {
                    raw: raw.to_string(),
                    req,
                }],
            };
        }

        Self::exact(raw)
    }

    fn is_any(&self) -> bool {
        self.exact.is_none() && self.ranges.is_empty()
    }

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(exact) = &self.exact {
            parts.push(format!("exact={exact}"));
        }
        if !self.ranges.is_empty() {
            parts.push(format!(
                "range={}",
                self.ranges
                    .iter()
                    .map(|range| range.raw.as_str())
                    .collect::<Vec<_>>()
                    .join(" && ")
            ));
        }
        if parts.is_empty() {
            "latest".to_string()
        } else {
            parts.join("; ")
        }
    }
}

#[derive(Debug, Deserialize)]
struct ModrinthVersion {
    id: String,
    project_id: Option<String>,
    version_number: String,
    name: String,
    #[serde(default)]
    date_published: Option<String>,
    files: Vec<ModrinthFile>,
    #[serde(default)]
    dependencies: Vec<ModrinthDependency>,
}

#[derive(Debug, Deserialize)]
struct ModrinthFile {
    hashes: HashMap<String, String>,
    url: String,
    filename: String,
    size: u64,
    primary: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ModrinthDependency {
    project_id: Option<String>,
    version_id: Option<String>,
    dependency_type: String,
}

#[derive(Debug, Deserialize)]
struct CurseforgeModSearchResponse {
    data: Vec<CurseforgeModSearchItem>,
}

#[derive(Debug, Clone, Deserialize)]
struct CurseforgeModSearchItem {
    id: u64,
    slug: String,
}

#[derive(Debug, Deserialize)]
struct CurseforgeFilesResponse {
    data: Vec<CurseforgeFile>,
}

#[derive(Debug, Clone, Deserialize)]
struct CurseforgeFile {
    id: u64,
    #[serde(rename = "fileName")]
    file_name: String,
    #[serde(rename = "fileDate")]
    file_date: Option<String>,
    #[serde(rename = "fileLength")]
    file_length: Option<u64>,
    #[serde(rename = "downloadUrl")]
    download_url: Option<String>,
    #[serde(rename = "gameVersions")]
    game_versions: Vec<String>,
    #[serde(default)]
    dependencies: Vec<CurseforgeDependency>,
    #[serde(default)]
    hashes: Vec<CurseforgeHash>,
}

#[derive(Debug, Clone, Deserialize)]
struct CurseforgeDependency {
    #[serde(rename = "modId")]
    mod_id: u64,
    #[serde(rename = "relationType")]
    relation_type: u8,
}

#[derive(Debug, Clone, Deserialize)]
struct CurseforgeHash {
    value: String,
    algo: u32,
}

#[derive(Debug, Deserialize)]
struct LoaderMetaEntry {
    loader: LoaderMetaVersion,
}

#[derive(Debug, Deserialize)]
struct LoaderMetaVersion {
    version: String,
    #[serde(default)]
    stable: Option<bool>,
}

pub fn resolve_lockfile(
    manifest: &Manifest,
    current: Option<&Lockfile>,
    request: &ResolveRequest,
) -> Result<ResolveOutput> {
    let packages = resolve_locked_packages(manifest, request)?;
    let plan = diff_packages(&packages, current, request);
    let lockfile = Lockfile::from_packages(manifest, packages);
    Ok(ResolveOutput { lockfile, plan })
}

pub fn resolve(
    manifest: &Manifest,
    current: Option<&Lockfile>,
    request: &ResolveRequest,
) -> Result<ResolutionPlan> {
    let packages = resolve_locked_packages(manifest, request)?;
    Ok(diff_packages(&packages, current, request))
}

pub fn resolve_loader_version(
    minecraft: &str,
    loader: LoaderKind,
    requested_version: &str,
) -> Result<String> {
    if !requested_version.eq_ignore_ascii_case("latest") {
        return Ok(requested_version.to_string());
    }

    let client = build_http_client()?;
    match loader {
        LoaderKind::Fabric => resolve_loader_version_from_meta(
            &client,
            FABRIC_LOADER_META_API,
            minecraft,
            "fabric-loader",
        ),
        LoaderKind::Quilt => resolve_loader_version_from_meta(
            &client,
            QUILT_LOADER_META_API,
            minecraft,
            "quilt-loader",
        ),
        LoaderKind::NeoForge => resolve_latest_neoforge_loader_version(&client, minecraft),
        LoaderKind::Forge => resolve_latest_forge_loader_version(&client, minecraft),
    }
}

fn resolve_loader_version_from_meta(
    client: &Client,
    api_base: &str,
    minecraft: &str,
    loader_name: &str,
) -> Result<String> {
    let url = format!("{api_base}/{minecraft}");
    let entries = send_with_retry(
        || client.get(&url),
        &format!("failed to query {loader_name} metadata"),
    )?
    .json::<Vec<LoaderMetaEntry>>()
    .with_context(|| format!("failed to decode {loader_name} metadata"))?;

    if let Some(stable) = entries.iter().find(|entry| {
        entry.loader.stable.unwrap_or(true) && !entry.loader.version.trim().is_empty()
    }) {
        return Ok(stable.loader.version.clone());
    }

    if let Some(first) = entries.first()
        && !first.loader.version.trim().is_empty()
    {
        return Ok(first.loader.version.clone());
    }

    bail!("no {loader_name} versions found for Minecraft {minecraft}")
}

fn resolve_latest_neoforge_loader_version(client: &Client, minecraft: &str) -> Result<String> {
    let body = fetch_text(
        client,
        NEOFORGE_MAVEN_METADATA_API,
        "NeoForge maven metadata",
    )?;
    let versions = parse_maven_metadata_versions(&body);
    if versions.is_empty() {
        bail!("NeoForge metadata does not contain version entries");
    }

    let branch = neoforge_branch_prefix(minecraft)
        .with_context(|| format!("unable to infer NeoForge branch for Minecraft {minecraft}"))?;

    let candidates: Vec<&str> = versions
        .iter()
        .map(String::as_str)
        .filter(|value| value.starts_with(branch.as_str()))
        .collect();
    let selected = select_highest_version_like(&candidates).with_context(|| {
        format!("no NeoForge versions found for Minecraft {minecraft} (branch {branch})")
    })?;
    Ok(selected.to_string())
}

fn resolve_latest_forge_loader_version(client: &Client, minecraft: &str) -> Result<String> {
    let body = fetch_text(client, FORGE_MAVEN_METADATA_API, "Forge maven metadata")?;
    let versions = parse_maven_metadata_versions(&body);
    if versions.is_empty() {
        bail!("Forge metadata does not contain version entries");
    }

    let exact_prefix = format!("{minecraft}-");
    let mut candidates: Vec<&str> = versions
        .iter()
        .map(String::as_str)
        .filter(|value| value.starts_with(exact_prefix.as_str()))
        .collect();

    if candidates.is_empty()
        && let Some(major_minor) = minecraft_major_minor(minecraft)
    {
        let fallback_prefix = format!("{major_minor}-");
        candidates = versions
            .iter()
            .map(String::as_str)
            .filter(|value| value.starts_with(fallback_prefix.as_str()))
            .collect();
    }

    let selected = select_highest_version_like(&candidates)
        .with_context(|| format!("no Forge versions found for Minecraft {minecraft}"))?;
    Ok(trim_forge_maven_version(selected).to_string())
}

fn fetch_text(client: &Client, url: &str, label: &str) -> Result<String> {
    send_with_retry(|| client.get(url), &format!("failed to query {label}"))?
        .text()
        .with_context(|| format!("failed to read {label} body"))
}

fn parse_maven_metadata_versions(xml: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut remaining = xml;
    let open = "<version>";
    let close = "</version>";

    while let Some(start) = remaining.find(open) {
        remaining = &remaining[start + open.len()..];
        let Some(end) = remaining.find(close) else {
            break;
        };
        let version = remaining[..end].trim();
        if !version.is_empty() {
            values.push(version.to_string());
        }
        remaining = &remaining[end + close.len()..];
    }

    values
}

fn neoforge_branch_prefix(minecraft: &str) -> Option<String> {
    let normalized = minecraft.strip_prefix("1.").unwrap_or(minecraft);
    let mut parts = normalized.split('.');
    let major = parts.next()?;
    let minor = parts.next().unwrap_or("0");
    if !major.chars().all(|ch| ch.is_ascii_digit()) || !minor.chars().all(|ch| ch.is_ascii_digit())
    {
        return None;
    }
    Some(format!("{major}.{minor}."))
}

fn minecraft_major_minor(minecraft: &str) -> Option<String> {
    let mut parts = minecraft.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    if !major.chars().all(|ch| ch.is_ascii_digit()) || !minor.chars().all(|ch| ch.is_ascii_digit())
    {
        return None;
    }
    Some(format!("{major}.{minor}"))
}

fn trim_forge_maven_version(value: &str) -> &str {
    value
        .split_once('-')
        .map(|(_, tail)| tail)
        .filter(|tail| !tail.trim().is_empty())
        .unwrap_or(value)
}

fn select_highest_version_like<'a>(versions: &[&'a str]) -> Option<&'a str> {
    versions
        .iter()
        .copied()
        .max_by(|left, right| compare_version_like(left, right))
}

fn compare_version_like(left: &str, right: &str) -> Ordering {
    let left_tokens = numeric_tokens(left);
    let right_tokens = numeric_tokens(right);
    let max_len = left_tokens.len().max(right_tokens.len());
    for idx in 0..max_len {
        let left_value = left_tokens.get(idx).copied().unwrap_or(0);
        let right_value = right_tokens.get(idx).copied().unwrap_or(0);
        let cmp = left_value.cmp(&right_value);
        if cmp != Ordering::Equal {
            return cmp;
        }
    }

    let stability = version_stability_rank(left).cmp(&version_stability_rank(right));
    if stability != Ordering::Equal {
        return stability;
    }
    left.cmp(right)
}

fn numeric_tokens(value: &str) -> Vec<u64> {
    value
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|token| !token.is_empty())
        .filter_map(|token| token.parse::<u64>().ok())
        .collect()
}

fn version_stability_rank(value: &str) -> u8 {
    let lower = value.to_ascii_lowercase();
    if lower.contains("alpha") {
        0
    } else if lower.contains("beta") {
        1
    } else if lower.contains("rc") {
        2
    } else {
        3
    }
}

fn resolve_locked_packages(
    manifest: &Manifest,
    request: &ResolveRequest,
) -> Result<Vec<LockedPackage>> {
    let client = build_http_client()?;
    let mut requirements: HashMap<String, ResolutionRequirement> = HashMap::new();
    let mut aliases: HashMap<String, String> = HashMap::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut resolved: HashMap<String, ResolvedEntry> = HashMap::new();
    let mut conflicts: Vec<String> = Vec::new();

    for group in active_groups(request) {
        let Some(mods) = manifest.group_mods(&group) else {
            continue;
        };

        for mod_spec in mods {
            let requirement = ResolutionRequirement {
                source: mod_spec.source,
                id: mod_spec.id.clone(),
                constraint: VersionConstraint::from_raw(&mod_spec.version),
                install_path: mod_spec.install_path.clone(),
                side: mod_spec.side,
                groups: root_group_set(&group),
                requested_by: manifest_request_chain(&group, mod_spec.source, &mod_spec.id),
            };

            if let Err(conflict) =
                insert_requirement(&mut requirements, &mut aliases, &mut queue, requirement)
            {
                conflicts.push(conflict);
            }
        }
    }

    while let Some(key) = queue.pop_front() {
        let Some(requirement) = requirements.get(&key).cloned() else {
            continue;
        };

        let resolved_entry = resolve_requirement(&client, manifest, &requirement)
            .with_context(|| format!("while resolving {}", requirement_label(&requirement)))?;

        if !requirement_matches_resolved(&requirement, &resolved_entry) {
            conflicts.push(format!(
                "{} does not satisfy constraint: {} (resolved to {})",
                requirement_label(&requirement),
                requirement.constraint.describe(),
                resolved_entry.package.version
            ));
        }

        if let Some(existing) = resolved.get(&key)
            && (existing.package.version != resolved_entry.package.version
                || existing.resolved_version_id != resolved_entry.resolved_version_id)
        {
            conflicts.push(format!(
                "{} resolved to conflicting versions: {} and {}",
                requirement.id, existing.package.version, resolved_entry.package.version
            ));
        }

        let canonical_key =
            requirement_key(resolved_entry.package.source, &resolved_entry.package.id);
        if canonical_key != key {
            if let Some(mapped) = aliases.get(&canonical_key)
                && mapped != &key
            {
                conflicts.push(format!(
                    "{canonical_key} aliases to both {mapped} and {key}"
                ));
            } else {
                aliases.insert(canonical_key, key.clone());
            }
        }

        for dependency in &resolved_entry.dependencies {
            let dep_alias_key = requirement_key(dependency.source, &dependency.id);
            let dep_key = aliases
                .get(&dep_alias_key)
                .cloned()
                .unwrap_or(dep_alias_key.clone());

            match dependency.kind {
                DependencyKind::Required => {
                    let dep_id = requirements
                        .get(&dep_key)
                        .map(|existing| existing.id.clone())
                        .unwrap_or_else(|| dependency.id.clone());
                    let dep_requirement = ResolutionRequirement {
                        source: dependency.source,
                        id: dep_id,
                        constraint: dependency.constraint.clone(),
                        install_path: None,
                        side: requirement.side,
                        groups: dependency.groups.clone(),
                        requested_by: dependency.requested_by.clone(),
                    };

                    if let Err(conflict) = insert_requirement(
                        &mut requirements,
                        &mut aliases,
                        &mut queue,
                        dep_requirement,
                    ) {
                        conflicts.push(conflict);
                    }

                    let actual_dep_key = aliases
                        .get(&dep_alias_key)
                        .cloned()
                        .unwrap_or(dep_key.clone());
                    if let (Some(dep_req), Some(existing_dep)) = (
                        requirements.get(&actual_dep_key),
                        resolved.get(&actual_dep_key),
                    ) && !requirement_matches_resolved(dep_req, existing_dep)
                    {
                        conflicts.push(format!(
                            "dependency {} required by {} has version conflict",
                            dependency.id, dependency.requested_by
                        ));
                    }
                }
                DependencyKind::Incompatible => {
                    if requirements.contains_key(&dep_key) || resolved.contains_key(&dep_key) {
                        conflicts.push(format!(
                            "{} declares incompatible dependency {}",
                            dependency.requested_by, dependency.id
                        ));
                    }
                }
            }
        }

        resolved.insert(key, resolved_entry);
    }

    if !conflicts.is_empty() {
        conflicts.sort();
        conflicts.dedup();
        bail!(
            "dependency conflict precheck failed:\n- {}",
            conflicts.join("\n- ")
        );
    }

    let mut keys: Vec<String> = resolved.keys().cloned().collect();
    keys.sort();

    let mut packages = Vec::with_capacity(keys.len());
    for key in keys {
        if let Some(entry) = resolved.remove(&key) {
            packages.push(entry.package);
        }
    }

    Ok(packages)
}

fn insert_requirement(
    requirements: &mut HashMap<String, ResolutionRequirement>,
    aliases: &mut HashMap<String, String>,
    queue: &mut VecDeque<String>,
    mut incoming: ResolutionRequirement,
) -> std::result::Result<(), String> {
    let alias_key = requirement_key(incoming.source, &incoming.id);
    if let Some(actual_key) = aliases.get(&alias_key).cloned()
        && let Some(existing) = requirements.get(&actual_key)
    {
        incoming.id = existing.id.clone();
        incoming.source = existing.source;
        incoming.install_path = existing.install_path.clone();
    }

    let (key, changed) = merge_requirement(requirements, incoming)?;
    aliases.insert(alias_key, key.clone());
    aliases.entry(key.clone()).or_insert_with(|| key.clone());
    if changed {
        queue.push_back(key);
    }

    Ok(())
}

fn merge_requirement(
    requirements: &mut HashMap<String, ResolutionRequirement>,
    incoming: ResolutionRequirement,
) -> std::result::Result<(String, bool), String> {
    let key = requirement_key(incoming.source, &incoming.id);

    if let Some(existing) = requirements.get_mut(&key) {
        let (merged_constraint, constraint_changed) = merge_constraints(
            &existing.constraint,
            &incoming.constraint,
            &existing.id,
            &existing.requested_by,
            &incoming.requested_by,
        )?;

        let merged_side = merge_side(existing.side, incoming.side);
        let side_changed = merged_side != existing.side;
        let (merged_install_path, install_path_changed) = merge_install_paths(
            existing.install_path.as_ref(),
            incoming.install_path.as_ref(),
        )
        .map_err(|err| {
            format!(
                "{} has conflicting install paths: {} ({})",
                existing.id, err, incoming.requested_by
            )
        })?;
        let groups_changed = merge_requirement_groups(&mut existing.groups, &incoming.groups);

        existing.constraint = merged_constraint;
        existing.side = merged_side;
        existing.install_path = merged_install_path;

        Ok((
            key,
            constraint_changed || side_changed || install_path_changed || groups_changed,
        ))
    } else {
        requirements.insert(key.clone(), incoming);
        Ok((key, true))
    }
}

fn merge_requirement_groups(current: &mut BTreeSet<String>, incoming: &BTreeSet<String>) -> bool {
    let before = current.len();
    current.extend(incoming.iter().cloned());
    current.len() != before
}

fn merge_install_paths(
    current: Option<&String>,
    incoming: Option<&String>,
) -> std::result::Result<(Option<String>, bool), String> {
    match (current.map(String::as_str), incoming.map(String::as_str)) {
        (Some(left), Some(right)) if left != right => Err(format!("{left} vs {right}")),
        (Some(left), _) => Ok((Some(left.to_string()), false)),
        (None, Some(right)) => Ok((Some(right.to_string()), true)),
        (None, None) => Ok((None, false)),
    }
}

fn merge_constraints(
    current: &VersionConstraint,
    incoming: &VersionConstraint,
    package_id: &str,
    current_by: &str,
    incoming_by: &str,
) -> std::result::Result<(VersionConstraint, bool), String> {
    let mut merged = current.clone();
    let mut changed = false;

    match (&merged.exact, &incoming.exact) {
        (Some(a), Some(b)) if a != b => {
            return Err(format!(
                "{package_id} has conflicting exact versions: {a} ({current_by}) vs {b} ({incoming_by})"
            ));
        }
        (None, Some(exact)) => {
            merged.exact = Some(exact.clone());
            changed = true;
        }
        _ => {}
    }

    for range in &incoming.ranges {
        if !merged
            .ranges
            .iter()
            .any(|existing| existing.raw == range.raw)
        {
            merged.ranges.push(range.clone());
            changed = true;
        }
    }

    if let Some(exact) = merged.exact.as_ref()
        && !exact_satisfies_ranges(exact, &merged.ranges)
    {
        return Err(format!(
            "{package_id} exact version {exact} does not satisfy range constraints"
        ));
    }

    Ok((merged, changed))
}

fn exact_satisfies_ranges(exact: &str, ranges: &[SemverRange]) -> bool {
    if ranges.is_empty() {
        return true;
    }

    let candidates = extract_semver_candidates(exact);
    if candidates.is_empty() {
        return false;
    }

    candidates
        .iter()
        .any(|candidate| ranges.iter().all(|range| range.req.matches(candidate)))
}

fn resolve_requirement(
    client: &Client,
    manifest: &Manifest,
    requirement: &ResolutionRequirement,
) -> Result<ResolvedEntry> {
    match requirement.source {
        ModSource::Modrinth => resolve_modrinth_requirement(client, manifest, requirement),
        ModSource::Curseforge => resolve_curseforge_requirement(client, manifest, requirement),
        ModSource::Url => resolve_url_requirement(requirement),
        ModSource::Local => resolve_local_requirement(requirement),
        ModSource::S3 => resolve_s3_requirement(manifest, requirement),
    }
}

fn resolve_modrinth_requirement(
    client: &Client,
    manifest: &Manifest,
    requirement: &ResolutionRequirement,
) -> Result<ResolvedEntry> {
    let loader = modrinth_loader(manifest);
    let loaders = format!("[\"{loader}\"]");
    let game_versions = format!("[\"{}\"]", manifest.project.minecraft);
    let endpoint = format!("{MODRINTH_PROJECT_VERSION_API}/{}/version", requirement.id);
    let response = send_with_retry(
        || {
            client.get(&endpoint).query(&[
                ("loaders", loaders.as_str()),
                ("game_versions", game_versions.as_str()),
            ])
        },
        &format!("failed to query Modrinth project {}", requirement.id),
    )?;

    let versions: Vec<ModrinthVersion> = response
        .json()
        .with_context(|| format!("failed to decode Modrinth versions for {}", requirement.id))?;

    let version = select_modrinth_version(&versions, &requirement.constraint)
        .with_context(|| format!("no matching Modrinth version for {}", requirement.id))?;

    let file = version
        .files
        .iter()
        .find(|entry| entry.primary.unwrap_or(false))
        .or_else(|| version.files.first())
        .context("selected Modrinth version has no downloadable files")?;

    let package_id = version
        .project_id
        .clone()
        .unwrap_or_else(|| requirement.id.clone());

    let mut package = LockedPackage {
        id: package_id.clone(),
        source: ModSource::Modrinth,
        version: version.version_number.clone(),
        side: requirement.side,
        file_name: file.filename.clone(),
        install_path: requirement.install_path.clone(),
        file_size: Some(file.size),
        sha256: "pending".to_string(),
        download_url: file.url.clone(),
        hashes: Vec::new(),
        source_ref: Some(format!(
            "requested={};project={};version={};name={}",
            requirement.id, package_id, version.id, version.name
        )),
        groups: package_groups(&requirement.groups),
        dependencies: Vec::new(),
    };

    for (algorithm, value) in &file.hashes {
        if let Some(algorithm) = parse_hash_algorithm(algorithm) {
            package.hashes.push(PackageHash {
                algorithm,
                value: value.clone(),
            });
        }
    }
    package.normalize_hashes();

    let mut dependencies = Vec::new();
    let request_chain = extend_request_chain(
        &requirement.requested_by,
        package.source,
        &package.id,
        &package.version,
    );
    for dependency in &version.dependencies {
        let Some(project_id) = dependency.project_id.clone() else {
            continue;
        };

        match dependency.dependency_type.as_str() {
            "required" => dependencies.push(DependencyEdge {
                source: ModSource::Modrinth,
                id: project_id,
                constraint: dependency
                    .version_id
                    .as_ref()
                    .map(|id| VersionConstraint::exact(id.clone()))
                    .unwrap_or_else(VersionConstraint::any),
                kind: DependencyKind::Required,
                groups: requirement.groups.clone(),
                requested_by: request_chain.clone(),
            }),
            "incompatible" => dependencies.push(DependencyEdge {
                source: ModSource::Modrinth,
                id: project_id,
                constraint: VersionConstraint::any(),
                kind: DependencyKind::Incompatible,
                groups: requirement.groups.clone(),
                requested_by: request_chain.clone(),
            }),
            _ => {}
        }
    }
    package.dependencies = dependencies
        .iter()
        .map(locked_dependency_from_edge)
        .collect();

    Ok(ResolvedEntry {
        package,
        resolved_version_id: Some(version.id.clone()),
        resolved_name: Some(version.name.clone()),
        dependencies,
    })
}

fn resolve_curseforge_requirement(
    client: &Client,
    manifest: &Manifest,
    requirement: &ResolutionRequirement,
) -> Result<ResolvedEntry> {
    let api_key = env::var("CURSEFORGE_API_KEY")
        .context("CURSEFORGE_API_KEY is required for curseforge resolution")?;

    let mod_id = resolve_curseforge_mod_id(client, &requirement.id, &api_key)?;

    let endpoint = format!("{CURSEFORGE_FILES_API}/{mod_id}/files");
    let response = send_with_retry(
        || {
            client
                .get(&endpoint)
                .query(&[
                    ("gameVersion", manifest.project.minecraft.as_str()),
                    ("pageSize", "50"),
                ])
                .header("x-api-key", &api_key)
        },
        &format!("failed to query CurseForge files for mod {mod_id}"),
    )?;

    let payload: CurseforgeFilesResponse = response
        .json()
        .context("failed to decode CurseForge files response")?;

    let mut candidates: Vec<CurseforgeFile> = payload
        .data
        .into_iter()
        .filter(|file| {
            file.game_versions
                .iter()
                .any(|value| value.eq_ignore_ascii_case(&manifest.project.minecraft))
        })
        .collect();

    if candidates.is_empty() {
        bail!(
            "no CurseForge file found for {} on minecraft {}",
            requirement.id,
            manifest.project.minecraft
        );
    }

    let loader_token = curseforge_loader_token(manifest);
    let loader_filtered: Vec<CurseforgeFile> = candidates
        .iter()
        .filter(|file| {
            file.game_versions
                .iter()
                .any(|value| value.eq_ignore_ascii_case(loader_token))
        })
        .cloned()
        .collect();

    if !loader_filtered.is_empty() {
        candidates = loader_filtered;
    }

    candidates.sort_by(|a, b| b.id.cmp(&a.id));

    if !requirement.constraint.is_any() {
        let filtered: Vec<CurseforgeFile> = candidates
            .iter()
            .filter(|file| {
                constraint_matches(
                    &requirement.constraint,
                    &file.file_name,
                    Some(file.id.to_string().as_str()),
                    None,
                )
            })
            .cloned()
            .collect();

        if filtered.is_empty() {
            bail!(
                "no CurseForge file satisfies constraint {} for {}",
                requirement.constraint.describe(),
                requirement.id
            );
        }

        candidates = filtered;
    }

    let selected = candidates
        .into_iter()
        .next()
        .context("no compatible CurseForge file matched requested version")?;

    let download_url = selected.download_url.clone().with_context(|| {
        format!(
            "CurseForge file {} does not expose direct download URL",
            selected.id
        )
    })?;

    let mut package = LockedPackage {
        id: mod_id.to_string(),
        source: ModSource::Curseforge,
        version: selected.id.to_string(),
        side: requirement.side,
        file_name: selected.file_name.clone(),
        install_path: requirement.install_path.clone(),
        file_size: selected.file_length,
        sha256: "pending".to_string(),
        download_url,
        hashes: Vec::new(),
        source_ref: Some(format!("mod={};file={}", mod_id, selected.id)),
        groups: package_groups(&requirement.groups),
        dependencies: Vec::new(),
    };

    for hash in &selected.hashes {
        if let Some(algorithm) = parse_curseforge_hash_algorithm(hash.algo) {
            package.hashes.push(PackageHash {
                algorithm,
                value: hash.value.clone(),
            });
        }
    }
    package.normalize_hashes();

    let mut dependencies = Vec::new();
    let request_chain = extend_request_chain(
        &requirement.requested_by,
        package.source,
        &package.id,
        &package.version,
    );
    for dependency in &selected.dependencies {
        match dependency.relation_type {
            3 => dependencies.push(DependencyEdge {
                source: ModSource::Curseforge,
                id: dependency.mod_id.to_string(),
                constraint: VersionConstraint::any(),
                kind: DependencyKind::Required,
                groups: requirement.groups.clone(),
                requested_by: request_chain.clone(),
            }),
            5 => dependencies.push(DependencyEdge {
                source: ModSource::Curseforge,
                id: dependency.mod_id.to_string(),
                constraint: VersionConstraint::any(),
                kind: DependencyKind::Incompatible,
                groups: requirement.groups.clone(),
                requested_by: request_chain.clone(),
            }),
            _ => {}
        }
    }
    package.dependencies = dependencies
        .iter()
        .map(locked_dependency_from_edge)
        .collect();

    Ok(ResolvedEntry {
        package,
        resolved_version_id: Some(selected.id.to_string()),
        resolved_name: Some(selected.file_name),
        dependencies,
    })
}

fn resolve_curseforge_mod_id(client: &Client, raw_id: &str, api_key: &str) -> Result<u64> {
    if let Ok(id) = raw_id.parse::<u64>() {
        return Ok(id);
    }

    let game_id = MINECRAFT_GAME_ID.to_string();
    let response = send_with_retry(
        || {
            client
                .get(CURSEFORGE_SEARCH_API)
                .query(&[
                    ("gameId", game_id.as_str()),
                    ("searchFilter", raw_id),
                    ("classId", "6"),
                    ("pageSize", "10"),
                ])
                .header("x-api-key", api_key)
        },
        &format!("failed to query CurseForge mod {raw_id}"),
    )?;

    let payload: CurseforgeModSearchResponse = response
        .json()
        .context("failed to decode CurseForge search response")?;
    payload
        .data
        .iter()
        .find(|entry| entry.slug == raw_id)
        .or_else(|| payload.data.first())
        .map(|entry| entry.id)
        .with_context(|| format!("CurseForge mod not found: {raw_id}"))
}

fn resolve_url_requirement(requirement: &ResolutionRequirement) -> Result<ResolvedEntry> {
    let Some(url) = requirement.constraint.exact.as_ref() else {
        bail!("url source requires exact URL (not latest/range)");
    };

    if !url.starts_with("https://") && !url.starts_with("http://") {
        bail!("url source expects http(s) URL in version field");
    }

    let file_name = url
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("mod.jar")
        .to_string();

    Ok(ResolvedEntry {
        package: LockedPackage {
            id: requirement.id.clone(),
            source: ModSource::Url,
            version: url.clone(),
            side: requirement.side,
            file_name,
            install_path: requirement.install_path.clone(),
            file_size: None,
            sha256: "pending".to_string(),
            download_url: url.clone(),
            hashes: Vec::new(),
            source_ref: Some(url.clone()),
            groups: package_groups(&requirement.groups),
            dependencies: Vec::new(),
        },
        resolved_version_id: Some(url.clone()),
        resolved_name: None,
        dependencies: Vec::new(),
    })
}

fn resolve_local_requirement(requirement: &ResolutionRequirement) -> Result<ResolvedEntry> {
    let Some(path) = requirement.constraint.exact.as_ref() else {
        bail!("local source requires exact file path (not latest/range)");
    };

    let file_name = std::path::Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("mod.jar")
        .to_string();

    Ok(ResolvedEntry {
        package: LockedPackage {
            id: requirement.id.clone(),
            source: ModSource::Local,
            version: path.clone(),
            side: requirement.side,
            file_name,
            install_path: requirement.install_path.clone(),
            file_size: None,
            sha256: "pending".to_string(),
            download_url: path.clone(),
            hashes: Vec::new(),
            source_ref: Some(path.clone()),
            groups: package_groups(&requirement.groups),
            dependencies: Vec::new(),
        },
        resolved_version_id: Some(path.clone()),
        resolved_name: None,
        dependencies: Vec::new(),
    })
}

fn resolve_s3_requirement(
    manifest: &Manifest,
    requirement: &ResolutionRequirement,
) -> Result<ResolvedEntry> {
    let config = manifest
        .sources
        .s3
        .as_ref()
        .context("s3 source is used but [sources.s3] is not configured in mineconda.toml")?;
    let Some(raw_key) = requirement.constraint.exact.as_ref() else {
        bail!("s3 source requires exact object key (not latest/range)");
    };

    let key = normalize_s3_key(raw_key, config)?;
    let file_name = std::path::Path::new(&key)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("mod.jar")
        .to_string();
    let download_url = build_s3_download_url(config, &key)?;

    Ok(ResolvedEntry {
        package: LockedPackage {
            id: requirement.id.clone(),
            source: ModSource::S3,
            version: key.clone(),
            side: requirement.side,
            file_name,
            install_path: requirement.install_path.clone(),
            file_size: None,
            sha256: "pending".to_string(),
            download_url,
            hashes: Vec::new(),
            source_ref: Some(format!("s3://{}/{}", config.bucket.trim(), key)),
            groups: package_groups(&requirement.groups),
            dependencies: Vec::new(),
        },
        resolved_version_id: Some(key),
        resolved_name: None,
        dependencies: Vec::new(),
    })
}

fn normalize_s3_key(raw: &str, config: &S3SourceConfig) -> Result<String> {
    let mut key = if let Some(without_scheme) = raw.strip_prefix("s3://") {
        let (bucket, key) = without_scheme
            .split_once('/')
            .with_context(|| format!("invalid s3 URI `{raw}`, expected s3://bucket/key"))?;
        if bucket != config.bucket {
            bail!(
                "s3 URI bucket `{bucket}` does not match configured bucket `{}`",
                config.bucket
            );
        }
        key
    } else {
        raw
    }
    .trim()
    .trim_start_matches('/')
    .to_string();

    if key.is_empty() {
        bail!("s3 object key cannot be empty");
    }

    if let Some(prefix) = config.key_prefix.as_deref() {
        let normalized_prefix = prefix.trim().trim_matches('/');
        if !normalized_prefix.is_empty() {
            if key == normalized_prefix {
                bail!("s3 object key resolves to prefix only, expected a file key");
            }
            if !key.starts_with(normalized_prefix) {
                key = format!("{normalized_prefix}/{key}");
            }
        }
    }

    Ok(key)
}

fn build_s3_download_url(config: &S3SourceConfig, key: &str) -> Result<String> {
    let bucket = config.bucket.trim();
    if bucket.is_empty() {
        bail!("sources.s3.bucket cannot be empty");
    }

    let encoded_key = encode_s3_key(key);
    if let Some(base) = config.public_base_url.as_deref()
        && !base.trim().is_empty()
    {
        return Ok(format!(
            "{}/{}",
            base.trim().trim_end_matches('/'),
            encoded_key
        ));
    }

    if let Some(endpoint) = config.endpoint.as_deref()
        && !endpoint.trim().is_empty()
    {
        let endpoint = endpoint.trim().trim_end_matches('/');
        if config.path_style {
            return Ok(format!("{endpoint}/{bucket}/{encoded_key}"));
        }
        return Ok(s3_virtual_host_url(endpoint, bucket, &encoded_key));
    }

    let region = config
        .region
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("us-east-1");
    if config.path_style {
        if region == "us-east-1" {
            return Ok(format!("https://s3.amazonaws.com/{bucket}/{encoded_key}"));
        }
        return Ok(format!(
            "https://s3.{region}.amazonaws.com/{bucket}/{encoded_key}"
        ));
    }

    if region == "us-east-1" {
        return Ok(format!("https://{bucket}.s3.amazonaws.com/{encoded_key}"));
    }
    Ok(format!(
        "https://{bucket}.s3.{region}.amazonaws.com/{encoded_key}"
    ))
}

fn s3_virtual_host_url(endpoint: &str, bucket: &str, encoded_key: &str) -> String {
    if let Some((scheme, rest)) = endpoint.split_once("://") {
        return format!(
            "{scheme}://{bucket}.{}/{}",
            rest.trim_start_matches('/'),
            encoded_key
        );
    }
    format!(
        "https://{bucket}.{}/{}",
        endpoint.trim_start_matches('/'),
        encoded_key
    )
}

fn encode_s3_key(key: &str) -> String {
    let mut encoded = String::with_capacity(key.len());
    for &byte in key.as_bytes() {
        let ch = byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~' | '/') {
            encoded.push(ch);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn requirement_matches_resolved(
    requirement: &ResolutionRequirement,
    resolved: &ResolvedEntry,
) -> bool {
    let version_id = resolved.resolved_version_id.as_deref();
    let version_name = resolved.resolved_name.as_deref();
    constraint_matches(
        &requirement.constraint,
        &resolved.package.version,
        version_id,
        version_name,
    )
}

fn constraint_matches(
    constraint: &VersionConstraint,
    resolved_version: &str,
    resolved_version_id: Option<&str>,
    resolved_name: Option<&str>,
) -> bool {
    let exact_ok = if let Some(exact) = constraint.exact.as_ref() {
        let expected = exact.to_ascii_lowercase();
        resolved_version == exact
            || resolved_version
                .to_ascii_lowercase()
                .contains(expected.as_str())
            || resolved_version_id
                .map(|value| {
                    value == exact || value.to_ascii_lowercase().contains(expected.as_str())
                })
                .unwrap_or(false)
            || resolved_name
                .map(|value| value.to_ascii_lowercase().contains(expected.as_str()))
                .unwrap_or(false)
    } else {
        true
    };

    if !exact_ok {
        return false;
    }

    if constraint.ranges.is_empty() {
        return true;
    }

    let candidates = {
        let mut values = vec![resolved_version.to_string()];
        if let Some(version_id) = resolved_version_id {
            values.push(version_id.to_string());
        }
        if let Some(name) = resolved_name {
            values.push(name.to_string());
        }
        extract_semver_candidates_from_values(&values)
    };

    if candidates.is_empty() {
        return false;
    }

    candidates.iter().any(|candidate| {
        constraint
            .ranges
            .iter()
            .all(|range| range.req.matches(candidate))
    })
}

fn merge_side(left: ModSide, right: ModSide) -> ModSide {
    if left == right { left } else { ModSide::Both }
}

fn requirement_label(requirement: &ResolutionRequirement) -> String {
    format!(
        "{}@{} via {}",
        requirement.id,
        requirement.source.as_str(),
        requirement.requested_by
    )
}

fn requirement_key(source: ModSource, id: &str) -> String {
    format!("{}@{}", id, source.as_str())
}

fn package_key(pkg: &LockedPackage) -> String {
    requirement_key(pkg.source, &pkg.id)
}

fn package_display_key(pkg: &LockedPackage) -> String {
    format!("{}@{}:{}", pkg.id, pkg.source.as_str(), pkg.version)
}

fn select_modrinth_version<'a>(
    versions: &'a [ModrinthVersion],
    constraint: &VersionConstraint,
) -> Option<&'a ModrinthVersion> {
    versions.iter().find(|version| {
        constraint_matches(
            constraint,
            &version.version_number,
            Some(version.id.as_str()),
            Some(version.name.as_str()),
        )
    })
}

fn looks_like_semver_range(raw: &str) -> bool {
    raw.chars()
        .any(|ch| matches!(ch, '^' | '~' | '>' | '<' | '*' | '|' | ',' | '='))
}

fn extract_semver_candidates_from_values(values: &[String]) -> Vec<Version> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for raw in values {
        extract_semver_candidates_impl(raw, &mut seen, &mut out);
    }

    out
}

fn extract_semver_candidates(raw: &str) -> Vec<Version> {
    extract_semver_candidates_from_values(&[raw.to_string()])
}

fn extract_semver_candidates_impl(raw: &str, seen: &mut HashSet<String>, out: &mut Vec<Version>) {
    let mut token = String::new();

    for ch in raw.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_digit() || ch == '.' {
            token.push(ch);
        } else if !token.is_empty() {
            push_semver_token(&token, seen, out);
            token.clear();
        }
    }
}

fn push_semver_token(token: &str, seen: &mut HashSet<String>, out: &mut Vec<Version>) {
    let trimmed = token.trim_matches('.');
    if trimmed.is_empty() {
        return;
    }

    let parts: Vec<&str> = trimmed.split('.').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        return;
    }

    if parts
        .iter()
        .any(|part| !part.chars().all(|ch| ch.is_ascii_digit()))
    {
        return;
    }

    let major = parts[0].parse::<u64>().ok();
    let minor = parts.get(1).and_then(|v| v.parse::<u64>().ok()).or(Some(0));
    let patch = parts.get(2).and_then(|v| v.parse::<u64>().ok()).or(Some(0));

    let Some(major) = major else {
        return;
    };
    let Some(minor) = minor else {
        return;
    };
    let Some(patch) = patch else {
        return;
    };

    let version = Version::new(major, minor, patch);
    let key = version.to_string();
    if seen.insert(key) {
        out.push(version);
    }
}

fn diff_packages(
    desired_packages: &[LockedPackage],
    current: Option<&Lockfile>,
    request: &ResolveRequest,
) -> ResolutionPlan {
    let mut plan = ResolutionPlan::default();
    let Some(current_lock) = current else {
        plan.install = desired_packages.to_vec();
        return plan;
    };

    let current_by_key: HashMap<String, &LockedPackage> = current_lock
        .packages
        .iter()
        .map(|pkg| (package_key(pkg), pkg))
        .collect();

    let desired_keys: HashSet<String> = desired_packages.iter().map(package_key).collect();

    for desired in desired_packages {
        match current_by_key.get(&package_key(desired)) {
            Some(existing)
                if !request.upgrade
                    && existing.version == desired.version
                    && existing.download_url == desired.download_url
                    && existing.file_name == desired.file_name
                    && existing.groups == desired.groups
                    && existing.dependencies == desired.dependencies =>
            {
                plan.unchanged.push(package_display_key(desired));
            }
            _ => plan.install.push(desired.clone()),
        }
    }

    for existing in &current_lock.packages {
        if !desired_keys.contains(&package_key(existing)) {
            plan.remove.push(package_display_key(existing));
        }
    }

    plan
}

fn modrinth_loader(manifest: &Manifest) -> &'static str {
    modrinth_loader_kind(manifest.project.loader.kind)
}

fn modrinth_loader_kind(kind: LoaderKind) -> &'static str {
    match kind {
        LoaderKind::Fabric => "fabric",
        LoaderKind::Forge => "forge",
        LoaderKind::NeoForge => "neoforge",
        LoaderKind::Quilt => "quilt",
    }
}

fn curseforge_loader_token(manifest: &Manifest) -> &'static str {
    curseforge_loader_token_kind(manifest.project.loader.kind)
}

fn curseforge_loader_token_kind(kind: LoaderKind) -> &'static str {
    match kind {
        LoaderKind::Fabric => "Fabric",
        LoaderKind::Forge => "Forge",
        LoaderKind::NeoForge => "NeoForge",
        LoaderKind::Quilt => "Quilt",
    }
}

fn parse_hash_algorithm(name: &str) -> Option<HashAlgorithm> {
    if name.eq_ignore_ascii_case("sha1") {
        return Some(HashAlgorithm::Sha1);
    }
    if name.eq_ignore_ascii_case("sha256") {
        return Some(HashAlgorithm::Sha256);
    }
    if name.eq_ignore_ascii_case("sha512") {
        return Some(HashAlgorithm::Sha512);
    }
    if name.eq_ignore_ascii_case("md5") {
        return Some(HashAlgorithm::Md5);
    }
    None
}

fn parse_curseforge_hash_algorithm(algo: u32) -> Option<HashAlgorithm> {
    match algo {
        1 => Some(HashAlgorithm::Sha1),
        2 => Some(HashAlgorithm::Md5),
        3 => Some(HashAlgorithm::Sha256),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchSource {
    Modrinth,
    Curseforge,
    Mcmod,
}

impl SearchSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Modrinth => "modrinth",
            Self::Curseforge => "curseforge",
            Self::Mcmod => "mcmod",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub source: SearchSource,
    pub query: String,
    pub limit: usize,
    pub page: usize,
    pub minecraft_version: Option<String>,
    pub loader: Option<LoaderKind>,
}

#[derive(Debug, Clone)]
pub struct InstallVersionsRequest {
    pub source: ModSource,
    pub id: String,
    pub limit: usize,
    pub minecraft_version: Option<String>,
    pub loader: Option<LoaderKind>,
}

#[derive(Debug, Clone)]
pub struct InstallVersion {
    pub value: String,
    pub label: String,
    pub published_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub id: String,
    pub slug: String,
    pub title: String,
    pub summary: String,
    pub source: SearchSource,
    pub downloads: Option<u64>,
    pub url: String,
    pub dependencies: Vec<String>,
    pub supported_side: Option<ModSide>,
    pub source_homepage: Option<String>,
    pub linked_modrinth_url: Option<String>,
    pub linked_curseforge_url: Option<String>,
    pub linked_github_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SearchCacheEntry {
    #[serde(default)]
    schema_version: u32,
    created_at_unix: u64,
    results: Vec<SearchResult>,
}

pub fn search_mods(request: &SearchRequest) -> Result<Vec<SearchResult>> {
    if request.query.trim().is_empty() {
        bail!("search query cannot be empty");
    }

    if request.limit == 0 {
        bail!("search limit must be greater than zero");
    }

    if request.page == 0 {
        bail!("search page must be greater than zero");
    }

    if let Some(cached) = read_search_cache(request)? {
        return Ok(cached);
    }

    let mut results = match request.source {
        SearchSource::Modrinth => search_modrinth(request),
        SearchSource::Curseforge => search_curseforge(request),
        SearchSource::Mcmod => search_mcmod(request),
    }?;

    if request.source == SearchSource::Mcmod {
        enrich_mcmod_search_links(&mut results)?;
    }

    let _ = write_search_cache(request, &results);
    Ok(results)
}

fn search_modrinth(request: &SearchRequest) -> Result<Vec<SearchResult>> {
    #[derive(Debug, Deserialize)]
    struct ModrinthResponse {
        hits: Vec<ModrinthHit>,
    }

    #[derive(Debug, Deserialize)]
    struct ModrinthHit {
        project_id: String,
        slug: String,
        title: String,
        description: Option<String>,
        downloads: Option<u64>,
        client_side: Option<String>,
        server_side: Option<String>,
    }

    let facets = modrinth_search_facets(request)?;
    let client = build_http_client()?;
    let limit = request.limit.to_string();
    let offset = request
        .page
        .saturating_sub(1)
        .saturating_mul(request.limit)
        .to_string();
    let response = send_with_retry(
        || {
            client.get(MODRINTH_SEARCH_API).query(&[
                ("query", request.query.as_str()),
                ("limit", limit.as_str()),
                ("offset", offset.as_str()),
                ("facets", facets.as_str()),
            ])
        },
        "failed to query Modrinth API",
    )?;

    let payload: ModrinthResponse = response
        .json()
        .context("failed to decode Modrinth API response")?;

    let results = payload
        .hits
        .into_iter()
        .map(|hit| {
            let slug = hit.slug;
            let summary = hit
                .description
                .unwrap_or_else(|| "(no description)".to_string());
            let supported_side = parse_modrinth_supported_side(
                hit.client_side.as_deref(),
                hit.server_side.as_deref(),
            )
            .or_else(|| infer_mod_side_from_text(&summary));
            SearchResult {
                id: hit.project_id,
                slug: slug.clone(),
                title: hit.title,
                summary,
                source: SearchSource::Modrinth,
                downloads: hit.downloads,
                url: format!("https://modrinth.com/mod/{slug}"),
                dependencies: Vec::new(),
                supported_side,
                source_homepage: Some(format!("https://modrinth.com/mod/{slug}")),
                linked_modrinth_url: Some(format!("https://modrinth.com/mod/{slug}")),
                linked_curseforge_url: None,
                linked_github_url: None,
            }
        })
        .collect();

    Ok(results)
}

pub fn list_install_versions(request: &InstallVersionsRequest) -> Result<Vec<InstallVersion>> {
    if request.limit == 0 {
        bail!("version list limit must be greater than zero");
    }
    let client = build_http_client()?;
    match request.source {
        ModSource::Modrinth => list_modrinth_install_versions(&client, request),
        ModSource::Curseforge => list_curseforge_install_versions(&client, request),
        _ => bail!("version selection is only supported for modrinth/curseforge sources"),
    }
}

fn list_modrinth_install_versions(
    client: &Client,
    request: &InstallVersionsRequest,
) -> Result<Vec<InstallVersion>> {
    let endpoint = format!("{MODRINTH_PROJECT_VERSION_API}/{}/version", request.id);
    let mut query_params = Vec::new();
    let loaders = request
        .loader
        .map(|loader| format!("[\"{}\"]", modrinth_loader_kind(loader)));
    let game_versions = request
        .minecraft_version
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("[\"{value}\"]"));
    if let Some(loaders) = loaders.as_deref() {
        query_params.push(("loaders", loaders));
    }
    if let Some(game_versions) = game_versions.as_deref() {
        query_params.push(("game_versions", game_versions));
    }

    let response = send_with_retry(
        || {
            let request_builder = client.get(&endpoint);
            if query_params.is_empty() {
                request_builder
            } else {
                request_builder.query(&query_params)
            }
        },
        &format!("failed to query Modrinth project {}", request.id),
    )?;

    let mut versions: Vec<ModrinthVersion> = response
        .json()
        .with_context(|| format!("failed to decode Modrinth versions for {}", request.id))?;
    versions.sort_by(|a, b| {
        compare_published_desc(a.date_published.as_deref(), b.date_published.as_deref())
            .then_with(|| b.id.cmp(&a.id))
    });
    let out = versions
        .into_iter()
        .take(request.limit)
        .map(|version| {
            let base_label = if version.name == version.version_number {
                version.version_number.clone()
            } else {
                format!("{}  ({})", version.version_number, version.name)
            };
            let label = format_install_version_label(base_label, version.date_published.as_deref());
            InstallVersion {
                value: version.id,
                label,
                published_at: version.date_published,
            }
        })
        .collect();
    Ok(out)
}

fn list_curseforge_install_versions(
    client: &Client,
    request: &InstallVersionsRequest,
) -> Result<Vec<InstallVersion>> {
    let api_key = env::var("CURSEFORGE_API_KEY")
        .context("CURSEFORGE_API_KEY is required for curseforge version selection")?;
    let mod_id = resolve_curseforge_mod_id(client, request.id.as_str(), &api_key)?;

    let endpoint = format!("{CURSEFORGE_FILES_API}/{mod_id}/files");
    let mut query_params = vec![("pageSize", "100".to_string())];
    if let Some(minecraft_version) = request.minecraft_version.as_deref()
        && !minecraft_version.trim().is_empty()
    {
        query_params.push(("gameVersion", minecraft_version.to_string()));
    }

    let response = send_with_retry(
        || {
            client
                .get(&endpoint)
                .query(&query_params)
                .header("x-api-key", &api_key)
        },
        &format!("failed to query CurseForge files for mod {mod_id}"),
    )?;
    let payload: CurseforgeFilesResponse = response
        .json()
        .context("failed to decode CurseForge files response")?;

    let mut candidates: Vec<CurseforgeFile> = payload.data;
    if let Some(minecraft_version) = request.minecraft_version.as_deref()
        && !minecraft_version.trim().is_empty()
    {
        candidates.retain(|file| {
            file.game_versions
                .iter()
                .any(|value| value.eq_ignore_ascii_case(minecraft_version))
        });
    }

    if let Some(loader) = request.loader {
        let loader_token = curseforge_loader_token_kind(loader);
        let loader_filtered: Vec<CurseforgeFile> = candidates
            .iter()
            .filter(|file| {
                file.game_versions
                    .iter()
                    .any(|value| value.eq_ignore_ascii_case(loader_token))
            })
            .cloned()
            .collect();
        if !loader_filtered.is_empty() {
            candidates = loader_filtered;
        }
    }

    candidates.sort_by(|a, b| {
        compare_published_desc(a.file_date.as_deref(), b.file_date.as_deref())
            .then_with(|| b.id.cmp(&a.id))
    });
    let out = candidates
        .into_iter()
        .take(request.limit)
        .map(|file| InstallVersion {
            value: file.id.to_string(),
            label: format_install_version_label(file.file_name, file.file_date.as_deref()),
            published_at: file.file_date,
        })
        .collect();
    Ok(out)
}

fn compare_published_desc(a: Option<&str>, b: Option<&str>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => b.cmp(a),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn format_install_version_label(base: String, published_at: Option<&str>) -> String {
    if let Some(date) = short_date(published_at) {
        format!("{base}  [{date}]")
    } else {
        base
    }
}

fn short_date(value: Option<&str>) -> Option<&str> {
    value.and_then(|raw| raw.get(0..10))
}

fn modrinth_search_facets(request: &SearchRequest) -> Result<String> {
    let mut facets = vec![vec!["project_type:mod".to_string()]];
    if let Some(minecraft_version) = request.minecraft_version.as_deref()
        && !minecraft_version.trim().is_empty()
    {
        facets.push(vec![format!("versions:{minecraft_version}")]);
    }
    if let Some(loader) = request.loader {
        facets.push(vec![format!("categories:{}", modrinth_loader_kind(loader))]);
    }
    serde_json::to_string(&facets).context("failed to encode modrinth search facets")
}

fn search_curseforge(request: &SearchRequest) -> Result<Vec<SearchResult>> {
    #[derive(Debug, Deserialize)]
    struct CurseforgeResponse {
        data: Vec<CurseforgeMod>,
    }

    #[derive(Debug, Deserialize)]
    struct CurseforgeMod {
        id: usize,
        slug: String,
        name: String,
        summary: Option<String>,
        #[serde(rename = "downloadCount")]
        download_count: Option<f64>,
        links: Option<CurseforgeLinks>,
    }

    #[derive(Debug, Deserialize)]
    struct CurseforgeLinks {
        #[serde(rename = "websiteUrl")]
        website_url: Option<String>,
    }

    let api_key = env::var("CURSEFORGE_API_KEY")
        .context("CURSEFORGE_API_KEY is required for curseforge search (set env var then retry)")?;

    let client = build_http_client()?;
    let game_id = MINECRAFT_GAME_ID.to_string();
    let page_size = request.limit.to_string();
    let index = request
        .page
        .saturating_sub(1)
        .saturating_mul(request.limit)
        .to_string();
    let mut query_params = vec![
        ("gameId", game_id.as_str()),
        ("searchFilter", request.query.as_str()),
        ("pageSize", page_size.as_str()),
        ("index", index.as_str()),
        ("classId", "6"),
    ];
    if let Some(minecraft_version) = request.minecraft_version.as_deref()
        && !minecraft_version.trim().is_empty()
    {
        query_params.push(("gameVersion", minecraft_version));
    }
    let response = send_with_retry(
        || {
            client
                .get(CURSEFORGE_SEARCH_API)
                .query(&query_params)
                .header("x-api-key", &api_key)
        },
        "failed to query CurseForge API",
    )?;

    let payload: CurseforgeResponse = response
        .json()
        .context("failed to decode CurseForge API response")?;

    let results = payload
        .data
        .into_iter()
        .map(|item| {
            let slug = item.slug;
            let summary = item
                .summary
                .unwrap_or_else(|| "(no description)".to_string());
            let url = item
                .links
                .and_then(|links| links.website_url)
                .unwrap_or_else(|| format!("https://www.curseforge.com/minecraft/mc-mods/{slug}"));

            SearchResult {
                id: item.id.to_string(),
                slug,
                title: item.name,
                summary: summary.clone(),
                source: SearchSource::Curseforge,
                downloads: item.download_count.map(|value| value as u64),
                url: url.clone(),
                dependencies: Vec::new(),
                supported_side: infer_mod_side_from_text(&summary),
                source_homepage: Some(url.clone()),
                linked_modrinth_url: None,
                linked_curseforge_url: Some(url),
                linked_github_url: None,
            }
        })
        .collect();

    Ok(results)
}

fn search_mcmod(request: &SearchRequest) -> Result<Vec<SearchResult>> {
    let client = build_http_client()?;
    let page = request.page.to_string();
    let response = send_with_retry(
        || {
            client
                .get(MCMOD_SEARCH_API)
                .query(&[
                    ("key", request.query.as_str()),
                    ("filter", "1"),
                    ("mold", "1"),
                    ("page", page.as_str()),
                ])
                .header("Accept-Language", "zh-CN,zh;q=0.9")
        },
        "failed to query mcmod.cn search",
    )?;

    let payload = response
        .text()
        .context("failed to decode mcmod.cn search response")?;

    if mcmod_requires_security_verification(&payload) {
        bail!(
            "mcmod.cn requires interactive security verification for this request. retry later or use --source modrinth/curseforge"
        );
    }

    Ok(parse_mcmod_search_results(&payload, request.limit))
}

fn read_search_cache(request: &SearchRequest) -> Result<Option<Vec<SearchResult>>> {
    let path = search_cache_path(request)?;
    if !path.exists() {
        return Ok(None);
    }

    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        Err(_) => return Ok(None),
    };
    let cache: SearchCacheEntry = match serde_json::from_slice(&raw) {
        Ok(cache) => cache,
        Err(_) => return Ok(None),
    };

    let age = unix_timestamp().saturating_sub(cache.created_at_unix);
    if age > SEARCH_CACHE_TTL_SECS {
        return Ok(None);
    }
    if cache.schema_version != SEARCH_CACHE_SCHEMA_VERSION {
        return Ok(None);
    }

    Ok(Some(cache.results))
}

fn write_search_cache(request: &SearchRequest, results: &[SearchResult]) -> Result<()> {
    let path = search_cache_path(request)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create search cache dir {}", parent.display()))?;
    }

    let cache = SearchCacheEntry {
        schema_version: SEARCH_CACHE_SCHEMA_VERSION,
        created_at_unix: unix_timestamp(),
        results: results.to_vec(),
    };
    let raw = serde_json::to_vec(&cache).context("failed to encode search cache")?;
    fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn search_cache_path(request: &SearchRequest) -> Result<PathBuf> {
    let cache_root = search_cache_root()?;
    let file_name = search_cache_file_name(request);
    Ok(cache_root.join(file_name))
}

fn search_cache_file_name(request: &SearchRequest) -> String {
    let mut file_name = format!(
        "{}-l{}-p{}-{}",
        request.source.as_str(),
        request.limit,
        request.page,
        sanitize_search_cache_key(&request.query)
    );
    if let Some(minecraft_version) = request.minecraft_version.as_deref()
        && !minecraft_version.trim().is_empty()
    {
        file_name.push_str("-mc");
        file_name.push_str(&sanitize_search_cache_key(minecraft_version));
    }
    if let Some(loader) = request.loader {
        file_name.push_str("-ld");
        file_name.push_str(&sanitize_search_cache_key(modrinth_loader_kind(loader)));
    }
    file_name.push_str(".json");
    file_name
}

fn search_cache_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("MINECONDA_SEARCH_CACHE_DIR") {
        return Ok(PathBuf::from(path));
    }

    if let Some(path) = env::var_os("MINECONDA_CACHE_DIR") {
        return Ok(PathBuf::from(path).join("search-results"));
    }

    if let Some(home) = env::var_os("MINECONDA_HOME") {
        return Ok(PathBuf::from(home).join("cache").join("search"));
    }

    let home = env::var_os("HOME").context("HOME is not set and MINECONDA_HOME is missing")?;
    Ok(PathBuf::from(home)
        .join(".mineconda")
        .join("cache")
        .join("search"))
}

fn sanitize_search_cache_key(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }

    if out.trim_matches('_').is_empty() {
        return "query".to_string();
    }

    out.truncate(80);
    out
}

fn enrich_mcmod_search_links(results: &mut [SearchResult]) -> Result<()> {
    let client = build_http_client()?;
    for item in results {
        if item.source != SearchSource::Mcmod {
            continue;
        }
        let Some(homepage) = item.source_homepage.clone() else {
            continue;
        };

        let response = client
            .get(&homepage)
            .header("Accept-Language", "zh-CN,zh;q=0.9")
            .send();
        let Ok(response) = response else {
            continue;
        };
        let Ok(response) = response.error_for_status() else {
            continue;
        };
        let Ok(payload) = response.text() else {
            continue;
        };
        if mcmod_requires_security_verification(&payload) {
            continue;
        }

        let (modrinth, curseforge, github) = parse_linked_sources_from_homepage(&client, &payload);
        if item.linked_modrinth_url.is_none() {
            item.linked_modrinth_url = modrinth;
        }
        if item.linked_curseforge_url.is_none() {
            item.linked_curseforge_url = curseforge;
        }
        if item.linked_github_url.is_none() {
            item.linked_github_url = github;
        }

        if let Some(best) = item
            .linked_modrinth_url
            .clone()
            .or_else(|| item.linked_curseforge_url.clone())
            .or_else(|| item.linked_github_url.clone())
        {
            item.url = best;
        } else {
            item.url = homepage;
        }
    }

    Ok(())
}

fn parse_linked_sources_from_homepage(
    client: &Client,
    html: &str,
) -> (Option<String>, Option<String>, Option<String>) {
    let mut modrinth = None;
    let mut curseforge = None;
    let mut github = None;

    for href in extract_all_hrefs(html) {
        let normalized = normalize_mcmod_url(&href);
        let resolved = resolve_mcmod_jump_link(client, &normalized).unwrap_or(normalized);
        if modrinth.is_none() && resolved.contains("modrinth.com") {
            modrinth = Some(resolved);
            continue;
        }
        if curseforge.is_none() && resolved.contains("curseforge.com") {
            curseforge = Some(resolved);
            continue;
        }
        if github.is_none() && resolved.contains("github.com") {
            github = Some(resolved);
        }
    }

    (modrinth, curseforge, github)
}

fn resolve_mcmod_jump_link(client: &Client, url: &str) -> Option<String> {
    if !url.contains("mcmod.cn") {
        return Some(url.to_string());
    }

    let looks_like_jump = url.contains("/link/")
        || url.contains("/links/")
        || url.contains("/go/")
        || url.contains("redirect");
    if !looks_like_jump {
        return None;
    }

    let response = client.get(url).send().ok()?;
    let final_url = response.url().to_string();
    Some(final_url)
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn parse_mcmod_search_results(payload: &str, limit: usize) -> Vec<SearchResult> {
    let list_start = payload.find("<div class=\"search-result-list\">");
    let Some(list_start) = list_start else {
        return Vec::new();
    };

    let rest = &payload[list_start..];
    let list_end = rest
        .find("<div class=\"search-result-pages\">")
        .unwrap_or(rest.len());
    let list_html = &rest[..list_end];

    let mut results = Vec::new();
    for item_html in list_html.split("<div class=\"result-item\">").skip(1) {
        if results.len() >= limit {
            break;
        }
        if let Some(item) = parse_mcmod_result_item(item_html) {
            results.push(item);
        }
    }
    results
}

fn parse_mcmod_result_item(item_html: &str) -> Option<SearchResult> {
    let (href, title_html) = extract_primary_class_anchor(item_html)?;
    let url = normalize_mcmod_url(&href);
    let id = extract_mcmod_class_id(&url).unwrap_or_else(|| url.clone());
    let title = normalize_text(&title_html);
    let (linked_modrinth_url, linked_curseforge_url, linked_github_url) =
        parse_mcmod_linked_sources(item_html);

    let body_html = extract_between(item_html, "<div class=\"body\">", "</div>").unwrap_or("");
    let summary = normalized_summary(body_html);

    Some(SearchResult {
        id: id.clone(),
        slug: id,
        title,
        summary: summary.clone(),
        source: SearchSource::Mcmod,
        downloads: None,
        url,
        dependencies: parse_mcmod_dependencies(body_html),
        supported_side: infer_mod_side_from_text(&summary),
        source_homepage: Some(href_to_mcmod_homepage(&href)),
        linked_modrinth_url,
        linked_curseforge_url,
        linked_github_url,
    })
}

fn parse_modrinth_supported_side(
    client_side: Option<&str>,
    server_side: Option<&str>,
) -> Option<ModSide> {
    let client = client_side.and_then(parse_modrinth_environment_support);
    let server = server_side.and_then(parse_modrinth_environment_support);

    match (client, server) {
        (Some(true), Some(true)) => Some(ModSide::Both),
        (Some(true), Some(false)) | (Some(true), None) => Some(ModSide::Client),
        (Some(false), Some(true)) | (None, Some(true)) => Some(ModSide::Server),
        (Some(false), Some(false)) => None,
        _ => None,
    }
}

fn parse_modrinth_environment_support(value: &str) -> Option<bool> {
    match value {
        "required" | "optional" => Some(true),
        "unsupported" => Some(false),
        "unknown" => None,
        _ => None,
    }
}

fn infer_mod_side_from_text(text: &str) -> Option<ModSide> {
    let normalized = text.to_lowercase();

    let has_client = normalized.contains("客户端")
        || normalized.contains("client-side")
        || normalized.contains("client side")
        || normalized.contains("client only");
    let has_server = normalized.contains("服务端")
        || normalized.contains("server-side")
        || normalized.contains("server side")
        || normalized.contains("server only");

    if normalized.contains("仅客户端")
        || normalized.contains("仅限客户端")
        || normalized.contains("只需客户端")
        || normalized.contains("客户端专用")
        || normalized.contains("client only")
    {
        return Some(ModSide::Client);
    }

    if normalized.contains("仅服务端")
        || normalized.contains("仅限服务端")
        || normalized.contains("只需服务端")
        || normalized.contains("服务端专用")
        || normalized.contains("server only")
    {
        return Some(ModSide::Server);
    }

    if normalized.contains("双端")
        || normalized.contains("客户端和服务端")
        || normalized.contains("客户端/服务端")
        || normalized.contains("both client and server")
        || normalized.contains("client and server")
        || (has_client && has_server)
    {
        return Some(ModSide::Both);
    }

    None
}

fn extract_primary_class_anchor(item_html: &str) -> Option<(String, String)> {
    let mut cursor = 0;
    while let Some(found) = item_html[cursor..].find("href=\"") {
        let href_start = cursor + found + "href=\"".len();
        let href_end = item_html[href_start..].find('"')? + href_start;
        let href = &item_html[href_start..href_end];

        let text_start = item_html[href_end..].find('>')? + href_end + 1;
        let text_end = item_html[text_start..].find("</a>")? + text_start;
        let text = &item_html[text_start..text_end];

        cursor = text_end + "</a>".len();

        if is_mcmod_mod_class_href(href) {
            return Some((href.to_string(), text.to_string()));
        }
    }
    None
}

fn parse_mcmod_dependencies(body_html: &str) -> Vec<String> {
    if !body_html.contains("前置")
        && !body_html.contains("依赖")
        && !body_html.contains("联动")
        && !body_html.contains("需要")
    {
        return Vec::new();
    }

    let mut dependencies = Vec::new();
    let mut cursor = 0;
    while let Some(found) = body_html[cursor..].find("href=\"") {
        let href_start = cursor + found + "href=\"".len();
        let Some(relative_end) = body_html[href_start..].find('"') else {
            break;
        };
        let href_end = href_start + relative_end;
        let href = &body_html[href_start..href_end];

        let Some(tag_end) = body_html[href_end..].find('>') else {
            break;
        };
        let text_start = href_end + tag_end + 1;
        let Some(text_relative_end) = body_html[text_start..].find("</a>") else {
            break;
        };
        let text_end = text_start + text_relative_end;
        cursor = text_end + "</a>".len();

        if !is_mcmod_mod_class_href(href) {
            continue;
        }

        let value = normalize_text(&body_html[text_start..text_end]);
        if !value.is_empty() && !dependencies.iter().any(|entry| entry == &value) {
            dependencies.push(value);
        }
    }
    dependencies
}

fn extract_between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let start_idx = text.find(start)? + start.len();
    let end_idx = text[start_idx..].find(end)? + start_idx;
    Some(&text[start_idx..end_idx])
}

fn parse_mcmod_linked_sources(item_html: &str) -> (Option<String>, Option<String>, Option<String>) {
    let mut modrinth = None;
    let mut curseforge = None;
    let mut github = None;

    for href in extract_all_hrefs(item_html) {
        let href = normalize_mcmod_url(&href);
        if modrinth.is_none() && href.contains("modrinth.com") {
            modrinth = Some(href);
            continue;
        }
        if curseforge.is_none() && href.contains("curseforge.com") {
            curseforge = Some(href);
            continue;
        }
        if github.is_none() && href.contains("github.com") {
            github = Some(href);
        }
    }

    (modrinth, curseforge, github)
}

fn href_to_mcmod_homepage(href: &str) -> String {
    let normalized = normalize_mcmod_url(href);
    if normalized.contains("mcmod.cn/class/") {
        normalized
    } else {
        "https://www.mcmod.cn/".to_string()
    }
}

fn extract_all_hrefs(html: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut cursor = 0;
    while let Some(found) = html[cursor..].find("href=\"") {
        let href_start = cursor + found + "href=\"".len();
        let Some(relative_end) = html[href_start..].find('"') else {
            break;
        };
        let href_end = href_start + relative_end;
        let href = html[href_start..href_end].trim();
        if !href.is_empty() {
            links.push(href.to_string());
        }
        cursor = href_end + 1;
    }
    links
}

fn normalize_mcmod_url(href: &str) -> String {
    if href.starts_with("//") {
        return format!("https:{href}");
    }
    if href.starts_with('/') {
        return format!("https://www.mcmod.cn{href}");
    }
    href.to_string()
}

fn is_mcmod_mod_class_href(href: &str) -> bool {
    let normalized = normalize_mcmod_url(href);
    let class_token = "/class/";
    let Some(start) = normalized.find(class_token) else {
        return false;
    };
    let tail = &normalized[start + class_token.len()..];
    if tail.starts_with("category/") {
        return false;
    }
    let Some(end) = tail.find(".html") else {
        return false;
    };
    let id = &tail[..end];
    !id.is_empty() && id.chars().all(|ch| ch.is_ascii_digit())
}

fn extract_mcmod_class_id(url: &str) -> Option<String> {
    let class_token = "/class/";
    let start = url.find(class_token)? + class_token.len();
    let tail = &url[start..];
    let end = tail.find(".html").unwrap_or(tail.len());
    let id = &tail[..end];
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

fn normalized_summary(body_html: &str) -> String {
    let summary = normalize_text(body_html);
    if summary.is_empty() {
        "(no description)".to_string()
    } else {
        summary
    }
}

fn normalize_text(input: &str) -> String {
    let decoded = decode_html_entities(input);
    let without_markers = strip_mcmod_bracket_markers(&decoded);
    let stripped = strip_html_tags(&without_markers);
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_html_entities(input: &str) -> String {
    input
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn strip_mcmod_bracket_markers(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut idx = 0;

    while idx < input.len() {
        let Some(open_relative) = input[idx..].find('[') else {
            output.push_str(&input[idx..]);
            break;
        };

        let open = idx + open_relative;
        output.push_str(&input[idx..open]);

        let Some(close_relative) = input[open..].find(']') else {
            output.push_str(&input[open..]);
            break;
        };
        let close = open + close_relative;
        let marker = &input[open + 1..close];
        let is_markup = (marker.contains('=')
            || (marker.contains(':') && !marker.chars().any(char::is_whitespace)))
            && marker.len() <= 64;
        if is_markup {
            idx = close + 1;
            continue;
        }

        output.push('[');
        idx = open + 1;
    }

    output
}

fn strip_html_tags(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ => {
                if !in_tag {
                    output.push(ch);
                }
            }
        }
    }
    output
}

fn mcmod_requires_security_verification(payload: &str) -> bool {
    payload.contains("安全验证")
        || payload.contains("访问验证")
        || payload.contains("人机验证")
        || payload.contains("验证码")
}

fn send_with_retry<F>(mut build: F, label: &str) -> Result<Response>
where
    F: FnMut() -> reqwest::blocking::RequestBuilder,
{
    let mut last_error = None;

    for attempt in 0..HTTP_RETRY_ATTEMPTS {
        match build().send() {
            Ok(response) => match response.error_for_status() {
                Ok(response) => return Ok(response),
                Err(err) => {
                    if attempt + 1 < HTTP_RETRY_ATTEMPTS
                        && err.status().is_some_and(should_retry_http_status)
                    {
                        last_error = Some(anyhow!("{label}: {err}"));
                        sleep(http_retry_backoff(attempt));
                        continue;
                    }
                    return Err(err).context(label.to_string());
                }
            },
            Err(err) => {
                if attempt + 1 < HTTP_RETRY_ATTEMPTS && should_retry_http_error(&err) {
                    last_error = Some(anyhow!("{label}: {err}"));
                    sleep(http_retry_backoff(attempt));
                    continue;
                }
                return Err(err).context(label.to_string());
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("{label}: request did not succeed")))
}

fn should_retry_http_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
            | StatusCode::INTERNAL_SERVER_ERROR
    )
}

fn should_retry_http_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect() || err.is_request()
}

fn http_retry_backoff(attempt: usize) -> Duration {
    match attempt {
        0 => Duration::from_millis(250),
        1 => Duration::from_millis(800),
        _ => Duration::from_millis(1500),
    }
}

fn build_http_client() -> Result<Client> {
    let mut builder = Client::builder()
        .user_agent(http_user_agent())
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(35));

    if env::var_os("MINECONDA_NO_PROXY")
        .map(|value| value != "0")
        .unwrap_or(false)
    {
        builder = builder.no_proxy();
    }

    let client = builder.build().context("failed to build HTTP client")?;
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_requirement_detects_conflicting_exact_versions() {
        let mut requirements = HashMap::new();
        let first = ResolutionRequirement {
            source: ModSource::Modrinth,
            id: "sodium".to_string(),
            constraint: VersionConstraint::exact("v1"),
            install_path: None,
            side: ModSide::Both,
            groups: root_group_set(DEFAULT_GROUP_NAME),
            requested_by: "manifest".to_string(),
        };
        let second = ResolutionRequirement {
            source: ModSource::Modrinth,
            id: "sodium".to_string(),
            constraint: VersionConstraint::exact("v2"),
            install_path: None,
            side: ModSide::Both,
            groups: root_group_set(DEFAULT_GROUP_NAME),
            requested_by: "dep:foo".to_string(),
        };

        let first_result = merge_requirement(&mut requirements, first);
        assert!(first_result.is_ok());
        let second_result = merge_requirement(&mut requirements, second);
        assert!(second_result.is_err());
    }

    #[test]
    fn merge_requirement_upgrades_latest_to_pinned() {
        let mut requirements = HashMap::new();
        let first = ResolutionRequirement {
            source: ModSource::Modrinth,
            id: "fabric-api".to_string(),
            constraint: VersionConstraint::any(),
            install_path: None,
            side: ModSide::Client,
            groups: root_group_set(DEFAULT_GROUP_NAME),
            requested_by: "manifest".to_string(),
        };
        let second = ResolutionRequirement {
            source: ModSource::Modrinth,
            id: "fabric-api".to_string(),
            constraint: VersionConstraint::exact("abc123"),
            install_path: None,
            side: ModSide::Server,
            groups: root_group_set(DEFAULT_GROUP_NAME),
            requested_by: "dep:sodium-extra".to_string(),
        };

        let _ = merge_requirement(&mut requirements, first).unwrap();
        let (_, changed) = merge_requirement(&mut requirements, second).unwrap();
        assert!(changed);

        let key = requirement_key(ModSource::Modrinth, "fabric-api");
        let merged = requirements.get(&key).unwrap();
        assert_eq!(merged.constraint.exact.as_deref(), Some("abc123"));
        assert_eq!(merged.side, ModSide::Both);
    }

    #[test]
    fn requirement_match_accepts_version_id() {
        let requirement = ResolutionRequirement {
            source: ModSource::Modrinth,
            id: "sodium".to_string(),
            constraint: VersionConstraint::exact("OihdIimA"),
            install_path: None,
            side: ModSide::Both,
            groups: root_group_set(DEFAULT_GROUP_NAME),
            requested_by: "manifest".to_string(),
        };
        let resolved = ResolvedEntry {
            package: LockedPackage {
                id: "sodium".to_string(),
                source: ModSource::Modrinth,
                version: "mc1.20.1-0.5.13-fabric".to_string(),
                side: ModSide::Both,
                file_name: "sodium.jar".to_string(),
                install_path: None,
                file_size: Some(1),
                sha256: "pending".to_string(),
                download_url: "https://example.com/sodium.jar".to_string(),
                hashes: Vec::new(),
                source_ref: None,
                groups: vec![DEFAULT_GROUP_NAME.to_string()],
                dependencies: Vec::new(),
            },
            resolved_version_id: Some("OihdIimA".to_string()),
            resolved_name: Some("Sodium".to_string()),
            dependencies: Vec::new(),
        };

        assert!(requirement_matches_resolved(&requirement, &resolved));
    }

    #[test]
    fn semver_range_constraint_matches_candidates() {
        let constraint = VersionConstraint::from_raw("^0.5.0");
        assert!(constraint_matches(
            &constraint,
            "mc1.20.1-0.5.13-fabric",
            Some("abc"),
            Some("Sodium")
        ));
        assert!(!constraint_matches(
            &constraint,
            "mc1.20.1-1.1.0-fabric",
            Some("abc"),
            Some("Sodium")
        ));
    }

    #[test]
    fn merge_constraints_rejects_exact_outside_range() {
        let range = VersionConstraint::from_raw("<0.5.0");
        let exact = VersionConstraint::exact("1.0.0");
        let merged = merge_constraints(&range, &exact, "demo", "manifest", "dependency");
        assert!(merged.is_err());
    }

    #[test]
    fn parse_mcmod_search_result_extracts_summary_and_dependencies() {
        let payload = r#"
        <div class="search-result-list">
            <div class="result-item">
                <div class="head">
                    <a target="_blank" href="https://www.mcmod.cn/class/12345.html"><em>Iris</em> Shaders</a>
                </div>
                <div class="body">
                    [h1=概述]一个光影模组，依赖
                    <a href="https://www.mcmod.cn/class/111.html">Sodium</a>
                    与
                    <a href="https://www.mcmod.cn/class/222.html">Fabric API</a>。
                    源码：
                    <a href="https://github.com/IrisShaders/Iris">GitHub</a>
                </div>
            </div>
        </div>
        <div class="search-result-pages"></div>
        "#;

        let results = parse_mcmod_search_results(payload, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source.as_str(), "mcmod");
        assert_eq!(results[0].id, "12345");
        assert_eq!(results[0].title, "Iris Shaders");
        assert!(results[0].summary.contains("一个光影模组"));
        assert_eq!(results[0].dependencies, vec!["Sodium", "Fabric API"]);
        assert_eq!(
            results[0].linked_github_url.as_deref(),
            Some("https://github.com/IrisShaders/Iris")
        );
    }

    #[test]
    fn parse_mcmod_search_result_ignores_non_mod_entries() {
        let payload = r#"
        <div class="search-result-list">
            <div class="result-item">
                <div class="head">
                    <a target="_blank" href="https://www.mcmod.cn/class/category/24-1.html"></a>
                    <a target="_blank" href="https://www.mcmod.cn/item/12.html">某个物品</a>
                </div>
                <div class="body">不是模组</div>
            </div>
            <div class="result-item">
                <div class="head">
                    <a target="_blank" href="https://www.mcmod.cn/item/12.html">某个物品</a>
                </div>
                <div class="body">不是模组</div>
            </div>
            <div class="result-item">
                <div class="head">
                    <a target="_blank" href="https://www.mcmod.cn/class/999.html">某个模组</a>
                </div>
                <div class="body">简介</div>
            </div>
        </div>
        <div class="search-result-pages"></div>
        "#;

        let results = parse_mcmod_search_results(payload, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "999");
    }

    #[test]
    fn search_rejects_zero_page() {
        let request = SearchRequest {
            source: SearchSource::Modrinth,
            query: "iris".to_string(),
            limit: 10,
            page: 0,
            minecraft_version: None,
            loader: None,
        };
        let result = search_mods(&request);
        assert!(result.is_err());
    }

    #[test]
    fn sanitize_search_cache_key_replaces_special_chars() {
        let value = sanitize_search_cache_key("Iris 光影 / v1.0?");
        assert_eq!(value, "Iris______v1.0_");
    }

    #[test]
    fn parse_mcmod_homepage_links_extracts_known_sources() {
        let client = build_http_client().unwrap();
        let html = r#"
        <a href="https://modrinth.com/mod/iris">Modrinth</a>
        <a href="https://www.curseforge.com/minecraft/mc-mods/oculus">CurseForge</a>
        <a href="https://github.com/IrisShaders/Iris">GitHub</a>
        "#;
        let (modrinth, curseforge, github) = parse_linked_sources_from_homepage(&client, html);
        assert_eq!(modrinth.as_deref(), Some("https://modrinth.com/mod/iris"));
        assert_eq!(
            curseforge.as_deref(),
            Some("https://www.curseforge.com/minecraft/mc-mods/oculus")
        );
        assert_eq!(
            github.as_deref(),
            Some("https://github.com/IrisShaders/Iris")
        );
    }

    #[test]
    fn infer_mod_side_from_text_supports_cn_and_en() {
        assert_eq!(
            infer_mod_side_from_text("仅客户端安装"),
            Some(ModSide::Client)
        );
        assert_eq!(
            infer_mod_side_from_text("server only utility"),
            Some(ModSide::Server)
        );
        assert_eq!(
            infer_mod_side_from_text("客户端和服务端都需要安装"),
            Some(ModSide::Both)
        );
    }

    #[test]
    fn parse_modrinth_supported_side_maps_environment_flags() {
        assert_eq!(
            parse_modrinth_supported_side(Some("required"), Some("required")),
            Some(ModSide::Both)
        );
        assert_eq!(
            parse_modrinth_supported_side(Some("optional"), Some("unsupported")),
            Some(ModSide::Client)
        );
        assert_eq!(
            parse_modrinth_supported_side(Some("unsupported"), Some("optional")),
            Some(ModSide::Server)
        );
    }

    #[test]
    fn retryable_http_statuses_cover_common_transient_failures() {
        assert!(should_retry_http_status(StatusCode::REQUEST_TIMEOUT));
        assert!(should_retry_http_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(should_retry_http_status(StatusCode::BAD_GATEWAY));
        assert!(!should_retry_http_status(StatusCode::FORBIDDEN));
    }

    #[test]
    fn mcmod_security_verification_detection_matches_known_markers() {
        assert!(mcmod_requires_security_verification(
            "请先完成安全验证后继续访问"
        ));
        assert!(mcmod_requires_security_verification("页面需要人机验证"));
        assert!(!mcmod_requires_security_verification("普通模组介绍页面"));
    }

    #[test]
    fn modrinth_search_facets_include_environment_filters() {
        let request = SearchRequest {
            source: SearchSource::Modrinth,
            query: "iris".to_string(),
            limit: 10,
            page: 1,
            minecraft_version: Some("1.21.1".to_string()),
            loader: Some(LoaderKind::NeoForge),
        };
        let facets = modrinth_search_facets(&request).expect("facets");
        assert_eq!(
            facets,
            r#"[["project_type:mod"],["versions:1.21.1"],["categories:neoforge"]]"#
        );
    }

    #[test]
    fn search_cache_file_name_includes_environment_filters() {
        let request = SearchRequest {
            source: SearchSource::Modrinth,
            query: "Iris Shader".to_string(),
            limit: 5,
            page: 2,
            minecraft_version: Some("1.21.1".to_string()),
            loader: Some(LoaderKind::NeoForge),
        };
        let file_name = search_cache_file_name(&request);
        assert_eq!(
            file_name,
            "modrinth-l5-p2-Iris_Shader-mc1.21.1-ldneoforge.json"
        );
    }

    #[test]
    fn normalize_s3_key_accepts_s3_uri_and_applies_prefix() {
        let config = S3SourceConfig {
            bucket: "mods-bucket".to_string(),
            region: Some("ap-southeast-1".to_string()),
            endpoint: None,
            public_base_url: None,
            key_prefix: Some("packs/dev".to_string()),
            path_style: false,
        };
        let key =
            normalize_s3_key("s3://mods-bucket/ferritecore/ferritecore.jar", &config).unwrap();
        assert_eq!(key, "packs/dev/ferritecore/ferritecore.jar");
    }

    #[test]
    fn normalize_s3_key_rejects_bucket_mismatch() {
        let config = S3SourceConfig {
            bucket: "mods-bucket".to_string(),
            region: None,
            endpoint: None,
            public_base_url: None,
            key_prefix: None,
            path_style: false,
        };
        let err = normalize_s3_key("s3://other-bucket/a.jar", &config)
            .expect_err("expected bucket mismatch");
        assert!(format!("{err:#}").contains("does not match configured bucket"));
    }

    #[test]
    fn build_s3_download_url_prefers_public_base_url() {
        let config = S3SourceConfig {
            bucket: "mods-bucket".to_string(),
            region: None,
            endpoint: Some("https://s3.example.com".to_string()),
            public_base_url: Some("https://cdn.example.com/mods".to_string()),
            key_prefix: None,
            path_style: false,
        };
        let url = build_s3_download_url(&config, "client/My Mod.jar").unwrap();
        assert_eq!(url, "https://cdn.example.com/mods/client/My%20Mod.jar");
    }

    #[test]
    fn build_s3_download_url_supports_path_style_endpoint() {
        let config = S3SourceConfig {
            bucket: "mods-bucket".to_string(),
            region: None,
            endpoint: Some("https://minio.local:9000".to_string()),
            public_base_url: None,
            key_prefix: None,
            path_style: true,
        };
        let url = build_s3_download_url(&config, "packs/dev/iris.jar").unwrap();
        assert_eq!(
            url,
            "https://minio.local:9000/mods-bucket/packs/dev/iris.jar"
        );
    }

    #[test]
    fn parse_maven_metadata_versions_extracts_all_entries() {
        let xml = r#"
        <metadata>
            <versioning>
                <versions>
                    <version>1.21.1-52.1.0</version>
                    <version>1.21.1-52.1.1</version>
                </versions>
            </versioning>
        </metadata>
        "#;
        let versions = parse_maven_metadata_versions(xml);
        assert_eq!(versions, vec!["1.21.1-52.1.0", "1.21.1-52.1.1"]);
    }

    #[test]
    fn neoforge_branch_prefix_maps_minecraft_version() {
        assert_eq!(neoforge_branch_prefix("1.21.1").as_deref(), Some("21.1."));
        assert_eq!(neoforge_branch_prefix("21.1").as_deref(), Some("21.1."));
    }

    #[test]
    fn trim_forge_maven_version_drops_minecraft_prefix() {
        assert_eq!(trim_forge_maven_version("1.21.1-52.1.0"), "52.1.0");
        assert_eq!(trim_forge_maven_version("47.2.0"), "47.2.0");
    }

    #[test]
    fn select_highest_version_like_prefers_latest_stable() {
        let versions = vec!["21.1.227-beta", "21.1.226", "21.1.227"];
        assert_eq!(select_highest_version_like(&versions), Some("21.1.227"));
    }

    #[test]
    fn resolve_loader_version_keeps_pinned_version() {
        let version = resolve_loader_version("1.21.1", LoaderKind::NeoForge, "21.1.220").unwrap();
        assert_eq!(version, "21.1.220");
    }

    #[test]
    fn compare_published_desc_prefers_newer_and_present_values() {
        assert_eq!(
            compare_published_desc(Some("2024-01-01T00:00:00Z"), Some("2023-12-31T23:59:59Z")),
            Ordering::Less
        );
        assert_eq!(
            compare_published_desc(Some("2024-01-01T00:00:00Z"), None),
            Ordering::Less
        );
        assert_eq!(
            compare_published_desc(None, Some("2024-01-01T00:00:00Z")),
            Ordering::Greater
        );
    }

    #[test]
    fn format_install_version_label_appends_short_date() {
        let label = format_install_version_label(
            "1.2.3 (release)".to_string(),
            Some("2026-04-20T09:08:07Z"),
        );
        assert_eq!(label, "1.2.3 (release)  [2026-04-20]");
        let no_date = format_install_version_label("1.2.3".to_string(), Some("invalid"));
        assert_eq!(no_date, "1.2.3");
    }

    #[test]
    fn resolve_lockfile_tracks_group_membership_for_selected_groups() {
        let manifest: Manifest = toml::from_str(
            r#"
[project]
name = "pack"
minecraft = "1.21.1"

[project.loader]
kind = "neo-forge"
version = "latest"

[[mods]]
id = "shared"
source = "local"
version = "vendor/shared.jar"
side = "both"

[groups.client]
mods = [
  { id = "shared", source = "local", version = "vendor/shared.jar", side = "client" },
  { id = "iris", source = "local", version = "vendor/iris.jar", side = "client" }
]
"#,
        )
        .expect("manifest should parse");

        let output = resolve_lockfile(
            &manifest,
            None,
            &ResolveRequest {
                upgrade: false,
                groups: BTreeSet::from(["client".to_string()]),
            },
        )
        .expect("resolve lockfile");

        assert_eq!(output.lockfile.packages.len(), 2);
        let shared = output
            .lockfile
            .packages
            .iter()
            .find(|package| package.id == "shared")
            .expect("shared package");
        assert_eq!(
            shared.groups,
            vec![DEFAULT_GROUP_NAME.to_string(), "client".to_string()]
        );

        let iris = output
            .lockfile
            .packages
            .iter()
            .find(|package| package.id == "iris")
            .expect("iris package");
        assert_eq!(iris.groups, vec!["client".to_string()]);
    }
}
