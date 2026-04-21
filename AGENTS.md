# mineconda Codex Context

## Purpose
`mineconda` is a Rust workspace for Minecraft modpack management, inspired by `uv`.

Primary capabilities implemented now:
- initialize modpack workspace (`init`)
- add/remove/search mods (`add/remove/search`)
- list/update/pin/cache maintenance commands (`ls/update/pin/cache`)
- project diagnostics (`doctor`)
- lock dependency graph with conflict pre-check (`lock`)
- sync lockfile packages to local `mods/` (`sync`, supports `--locked/--frozen/--offline/--jobs/--verbose-cache`)
- manage Java runtime as environment (`env`)
- run dev instance in client/server/both mode (`run`, loader-aware launcher autodetect)
- import/export Modrinth `.mrpack` as the stable modpack format path
- export compatibility-oriented CurseForge/MultiMC metadata
- support project-level experimental S3 mod source (`[sources.s3]` + `add --source s3`)
- support project-level experimental S3 mod cache backend (`[cache.s3]`, read-through + backfill on `sync`, `auth=auto|anonymous|sigv4`)

## Workspace Structure
- `crates/mineconda-cli`: CLI entry, command orchestration
- `crates/mineconda-core`: manifest/lockfile data model and IO
- `crates/mineconda-resolver`: source search + dependency resolution + conflict checks
- `crates/mineconda-sync`: package download/cache/install sync
- `crates/mineconda-runtime`: managed Java runtime install/lookup
- `crates/mineconda-runner`: dev game instance run planning and process launching
- `crates/mineconda-export`: export to modpack formats
- `docs/`: architecture notes
- `scripts/ci-smoke.sh`: end-to-end smoke test script
- `.github/workflows/test.yml`: CI test pipeline
- `.github/workflows/s3-smoke.yml`: self-hosted/manual S3 smoke pipeline

## Project Files
- `mineconda.toml`: desired project state
- `mineconda.lock`: resolved reproducible state

## Test Pipeline
Keep local and CI steps consistent:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p mineconda-cli --release
MINECONDA_BIN="$(pwd)/target/release/mineconda" bash scripts/ci-smoke.sh
```

Optional enhanced S3 validation:

```bash
MINECONDA_ENABLE_S3_SMOKE=1 \
MINECONDA_S3_REMOTE_PRIVILEGE_SECRET='<remote-privilege-secret>' \
MINECONDA_S3_SSH_TARGET='<remote-ssh-target>' \
MINECONDA_BIN="$(pwd)/target/release/mineconda" \
bash scripts/ci-smoke.sh
```

CI workflow:
- file: `.github/workflows/test.yml`
- triggers: `push`, `pull_request`, `workflow_dispatch`
- checks: format, clippy, unit/doc tests, release binary build, smoke

## Smoke Test Coverage
`bash scripts/ci-smoke.sh` currently validates:
- `init` with `minecraft=1.21.1`, `loader=neoforge`
- `search iris --page 2` search path and pagination (default source = `modrinth`)
- repeated `search` hit local cache path
- `search embeddium --install-first` import mod from search result
- interactive `search ferritecore` via PTY + `Enter` validates `结果 -> 版本页 -> 安装` 路径
- interactive `search ferritecore` via PTY + `L` validates quick-install path
- add JEI via local source fixture (`add jei --source local`)
- `sync --jobs 2 --verbose-cache` install path and cache source output
- `cache stats` / `cache verify` local cache observability
- warmed-cache `sync --offline` restore path
- optional `source=s3` + private `cache.s3(auth=sigv4)` + `cache remote-prune --s3` smoke via remote experimental S3 target (`MINECONDA_ENABLE_S3_SMOKE=1`)
- `env install/use/list/which` managed Java runtime path
- `doctor` managed runtime path plus non-blocking experimental S3 diagnostics
- `ls --status --info`, `update`, `pin`, `cache` command chain
- `sync` install path and lockfile metadata update
- `sync --locked` reproducibility guard path
- `run --dry-run` plus real `run` execution for `client`, `server`, `both`
- explicit server `unix_args.txt` launch path for installed NeoForge-style servers
- `export --format mrpack`, `export --format curseforge`, and `export --format mods-desc`
- `export --format curseforge` compatibility warning path
- exported `mrpack/curseforge` loader versions are resolved (not `latest`)
- `import <mrpack>` auto-detect format path (`modrinth.index.json`) and strict mrpack import
- `import` rejects non-`.mrpack` ZIP archives with explicit unsupported-format error
- `import` supports local file + URL input; smoke validates both paths
- imported `mrpack files[].path` non-`mods/` entries are persisted and installed by `sync` (offline cache path)

`cargo test --workspace` additionally validates:
- resolver fixture lockfile snapshot (`crates/mineconda-resolver/tests/fixtures/local-pack`)
- export metadata behaviors (`crates/mineconda-export` unit tests)
- export format fixture snapshots (`crates/mineconda-export/tests/fixtures/export-pack`)

Smoke workspace:
- `.test/ci-smoke/`

## Codex Execution Notes
- prefer `rg` for search and `cargo` for build/test
- before merge, run full pipeline in order (fmt -> clippy -> test -> smoke)
- avoid relying on external APIs in baseline smoke checks
- keep `clippy -D warnings` clean (current standard)
- remove or generalize any detail that could expose the maintainer's local or test environment before committing or pushing:
  - host aliases
  - machine-specific paths
  - runner labels
  - fixed local credentials
  - internal network addresses
  - environment-specific log wording
- optional remote S3 smoke entry: `scripts/s3-smoke-remote.sh`
- real NeoForge server smoke entry: `scripts/actual-neoforge-server-smoke.sh`
- S3 smoke is local/self-hosted only; CI baseline does not inject S3 smoke env
- treat S3 as experimental in user-facing behavior and diagnostics unless explicitly validating that path
- `cache remote-prune --s3` currently belongs to enhanced S3 smoke, not baseline smoke
