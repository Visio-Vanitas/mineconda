use std::path::PathBuf;

use anyhow::Result;
use mineconda_core::{Lockfile, manifest_path, read_lockfile, read_manifest};
use mineconda_resolver::{ResolveRequest, resolve_lockfile};

#[test]
fn local_fixture_resolves_to_golden_lockfile() -> Result<()> {
    let fixture_root = fixture_root();
    let manifest = read_manifest(&manifest_path(&fixture_root))?;
    let expected = read_lockfile(&fixture_root.join("expected.lock.toml"))?;
    let actual = resolve_lockfile(&manifest, None, &ResolveRequest::default())?.lockfile;

    assert_lockfile_eq(actual, expected)?;
    Ok(())
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/local-pack")
}

fn assert_lockfile_eq(mut actual: Lockfile, mut expected: Lockfile) -> Result<()> {
    // generated_at is time-dependent, normalize before golden comparison.
    actual.metadata.generated_at_unix = 0;
    expected.metadata.generated_at_unix = 0;

    let actual = serde_json::to_value(actual)?;
    let expected = serde_json::to_value(expected)?;
    assert_eq!(actual, expected);
    Ok(())
}
