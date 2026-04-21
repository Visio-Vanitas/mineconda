use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use mineconda_core::{manifest_path, read_lockfile, read_manifest};
use mineconda_export::{ExportFormat, ExportRequest, export_pack};
use serde_json::Value;
use zip::ZipArchive;

#[test]
fn export_outputs_match_fixture_snapshots() -> Result<()> {
    let fixture_root = fixture_root();
    let expected_root = fixture_root.join("expected");
    let manifest = read_manifest(&manifest_path(&fixture_root))?;
    let lock = read_lockfile(&fixture_root.join("mineconda.lock"))?;
    let workspace = TempWorkspace::new("export-snapshot");

    assert_zip_json_snapshot(
        &manifest,
        &lock,
        &workspace.root,
        ExportFormat::Mrpack,
        "snapshot-mrpack",
        "modrinth.index.json",
        &expected_root.join("modrinth.index.json"),
    )?;
    assert_zip_json_snapshot(
        &manifest,
        &lock,
        &workspace.root,
        ExportFormat::CurseforgeZip,
        "snapshot-curseforge",
        "manifest.json",
        &expected_root.join("curseforge.manifest.json"),
    )?;
    assert_zip_json_snapshot(
        &manifest,
        &lock,
        &workspace.root,
        ExportFormat::MultiMcZip,
        "snapshot-multimc",
        "mmc-pack.json",
        &expected_root.join("multimc.pack.json"),
    )?;
    assert_plain_json_snapshot(
        &manifest,
        &lock,
        &workspace.root,
        ExportFormat::ModsDescriptionJson,
        "snapshot-mods-desc",
        &expected_root.join("mods-desc.json"),
    )?;

    Ok(())
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/export-pack")
}

fn assert_zip_json_snapshot(
    manifest: &mineconda_core::Manifest,
    lock: &mineconda_core::Lockfile,
    output_root: &Path,
    format: ExportFormat,
    output_stem: &str,
    zip_entry_name: &str,
    expected_json: &Path,
) -> Result<()> {
    let output_base = output_root.join(output_stem);
    let exported = export_pack(
        manifest,
        lock,
        &ExportRequest {
            output: output_base,
            format,
            project_root: None,
        },
    )?;
    let mut zip = ZipArchive::new(
        File::open(&exported).with_context(|| format!("failed to open {}", exported.display()))?,
    )
    .with_context(|| format!("failed to parse {}", exported.display()))?;
    let mut entry = zip
        .by_name(zip_entry_name)
        .with_context(|| format!("missing zip entry {zip_entry_name}"))?;

    let mut actual_raw = String::new();
    entry
        .read_to_string(&mut actual_raw)
        .context("failed to read zip json payload")?;
    let actual: Value = serde_json::from_str(&actual_raw).context("failed to decode zip json")?;
    let expected: Value = serde_json::from_str(
        &fs::read_to_string(expected_json)
            .with_context(|| format!("failed to read {}", expected_json.display()))?,
    )
    .with_context(|| format!("failed to decode {}", expected_json.display()))?;
    assert_eq!(
        actual,
        expected,
        "zip snapshot mismatch: {} in {}",
        zip_entry_name,
        exported.display()
    );
    Ok(())
}

fn assert_plain_json_snapshot(
    manifest: &mineconda_core::Manifest,
    lock: &mineconda_core::Lockfile,
    output_root: &Path,
    format: ExportFormat,
    output_stem: &str,
    expected_json: &Path,
) -> Result<()> {
    let output_base = output_root.join(output_stem);
    let exported = export_pack(
        manifest,
        lock,
        &ExportRequest {
            output: output_base,
            format,
            project_root: None,
        },
    )?;
    let actual: Value = serde_json::from_str(
        &fs::read_to_string(&exported)
            .with_context(|| format!("failed to read {}", exported.display()))?,
    )
    .with_context(|| format!("failed to decode {}", exported.display()))?;
    let expected: Value = serde_json::from_str(
        &fs::read_to_string(expected_json)
            .with_context(|| format!("failed to read {}", expected_json.display()))?,
    )
    .with_context(|| format!("failed to decode {}", expected_json.display()))?;

    assert_eq!(
        actual,
        expected,
        "json snapshot mismatch: {}",
        exported.display()
    );
    Ok(())
}

struct TempWorkspace {
    root: PathBuf,
}

impl TempWorkspace {
    fn new(tag: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("mineconda-export-{tag}-{unique}"));
        fs::create_dir_all(&root).expect("failed to create temp workspace");
        Self { root }
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
