# mineconda

English | [简体中文](README.zh-CN.md)

> Documentation note: parts of this documentation were written with GPT-5.4 and may contain outdated details or awkward phrasing. When in doubt, prefer the CLI help output and current code behavior.

`mineconda` is a Rust CLI for Minecraft modpack management, inspired by `uv`.

It provides a manifest + lockfile workflow for reproducible modpack environments, with dependency resolution, cache-aware sync, runtime management, and modpack import/export.

## Why mineconda

- **Reproducible**: `mineconda.toml` + `mineconda.lock`
- **Fast workflow**: declarative add/remove, lock, sync
- **Source flexibility**: Modrinth / CurseForge / mcmod search + URL / local, with experimental S3 source/cache support
- **Runtime-aware**: managed Java runtime via `mineconda env`
- **Pack-ready**: import/export for common formats (currently strict `.mrpack` support)

## Current Status

The project is actively evolving. Core workflows are already usable:

- `init`, `add`, `remove`, `ls`
- `search` (interactive/non-interactive, install from results)
- `group`, `tree`, `why`
- `lock`, `status`, `sync`, `cache`, `doctor`
- `env`, `run`, `import`, `export`

Stable baseline today:

- `search` / `add` / `lock` / `sync` / `run`
- `import` for Modrinth `.mrpack`
- `export` for Modrinth `.mrpack`

Compatibility / experimental areas:

- `export --format curseforge`
- `export --format multimc`
- `[sources.s3]` and `[cache.s3]`

## Installation

### Build from source

```bash
cargo build -p mineconda-cli --release
./target/release/mineconda --help
```

## Quick Start

```bash
# 1) initialize project
mineconda init mypack --minecraft 1.21.1 --loader neoforge

# 2) search and install first result
mineconda search embeddium --install-first --non-interactive

# 3) inspect current state
mineconda ls --status --info

# 4) sync locked packages into workspace
mineconda sync

# 5) run a dev instance (dry-run)
mineconda run --mode client --dry-run
```

## CLI Overview

```text
mineconda [--root <PATH>] [--member <MEMBER>] [--profile <NAME>] [--no-color] [--lang <auto|en|zh-cn>] <COMMAND>
```

Main commands:

- `init` / `add` / `remove` / `ls`
- `group` / `tree` / `why`
- `profile` / `workspace`
- `search` / `update` / `pin` / `lock` / `status`
- `sync` / `cache` / `doctor`
- `env` / `run`
- `import` / `export`

Useful package-state commands:

- `mineconda lock diff` previews lockfile changes without writing them
- `mineconda lock --check` validates the selected lock surface without rewriting `mineconda.lock`
- `mineconda status` reports manifest/lock/sync drift for the selected groups
- `mineconda sync --check` validates whether the selected locked packages are installed without mutating the workspace
- workspace roots additionally support `mineconda --all-members lock`, `lock --check`, and `sync --check`
- add `--json` to either command for machine-readable output and stable `0/2/1` exit codes
- `mineconda ls --json`, `mineconda tree --json`, and `mineconda why <id> --json` expose structured package graph data for tooling

## Dependency Groups

`mineconda` supports named dependency groups for splitting one project into multiple install
surfaces, similar to optional dependency groups in `uv`.

Model:

- top-level `mods = [...]` is the default group: `default`
- optional groups live under `[groups.<name>]`
- group names must be lowercase kebab-case
- commands activate only `default` unless you pass `--group <name>` or `--all-groups`

Example:

```toml
[project]
name = "mypack"
minecraft = "1.21.1"

[project.loader]
kind = "neo-forge"
version = "21.1.227"

mods = [
  { id = "jei", source = "modrinth", version = "latest", side = "both" }
]

[groups.client]
mods = [
  { id = "iris", source = "modrinth", version = "latest", side = "client" }
]

[groups.dev]
mods = [
  { id = "spark", source = "modrinth", version = "latest", side = "both" }
]
```

Typical workflow:

```bash
# add to the default group
mineconda add jei

# create and populate an extra group
mineconda group add client
mineconda add iris --group client

# inspect one group
mineconda ls --group client
mineconda tree --group client
mineconda why iris --group client

# resolve all groups together
mineconda lock --all-groups

# sync default + client for a local dev instance
mineconda sync --group client
mineconda run --mode client --group client
```

Notes:

- selecting any extra group always includes `default`
- `lock`, `sync`, `tree`, `why`, `run`, and `export` support `--group` / `--all-groups`
- old lockfiles may not contain group metadata; rerun `mineconda lock` if a group-aware
  command asks for it
- `run --mode client|server|both` does not auto-select groups; group activation is always
  explicit

## Profiles

Profiles are named aliases for group selections. They make repeated commands less noisy, similar
to reusable dependency surfaces in `uv` workflows.

Example:

```toml
[profiles.client-dev]
groups = ["client", "dev"]
```

Usage:

```bash
mineconda profile add client-dev --group client --group dev
mineconda sync --profile client-dev
mineconda run --profile client-dev --mode client
mineconda tree --profile client-dev
```

Rules:

- project profiles live in `mineconda.toml`
- workspace profiles live in `mineconda-workspace.toml`
- member-local profiles override workspace profiles with the same name
- `--profile` and `--group` are merged; `default` remains active

## Workspace

`mineconda` can manage multiple pack members from one workspace root.

Workspace file:

```toml
members = ["packs/client", "packs/server"]

[workspace]
name = "demo"

[profiles.client-dev]
groups = ["client", "dev"]
```

Typical workflow:

```bash
mineconda workspace init demo
mineconda workspace add packs/client
mineconda workspace add packs/server

mineconda --member client init client-pack --minecraft 1.21.1 --loader neoforge
mineconda --member client add jei
mineconda --member client lock

mineconda --all-members status
mineconda --all-members lock diff --json
```

Current workspace boundary:

- each member keeps its own `mineconda.toml` and `mineconda.lock`
- `status` and `lock diff` support `--all-members` aggregation
- `lock`, `lock --check`, `sync`, `sync --check`, `export`, and `run` support `--all-members` aggregation
- `--all-members export` writes one artifact per member next to the requested output path, suffixed with a deterministic member tag to avoid collisions
- `--all-members run` executes members sequentially in workspace order; `both` mode remains scoped inside each member run

## JSON Output

Machine-readable output is available for:

- `mineconda lock diff --json`
- `mineconda status --json`
- `mineconda ls --json`
- `mineconda tree --json`
- `mineconda why <id> --json`

When `--all-members` is used from a workspace root, `status --json` and `lock diff --json`
return per-member reports with aggregate exit codes.

## Search UX

- Interactive mode is enabled by default in TTY.
- Keys:
  - `↑/↓` or `j/k`: move selection
  - `Enter` / `V`: open version picker and install
  - `L`: quick-install latest compatible version
  - `q` / `Esc`: quit

Language selection:

- CLI flag: `--lang auto|en|zh-cn`
- Env var: `MINECONDA_LANG`
- Priority: `--lang` > `MINECONDA_LANG` > system locale

## Configuration Highlights

- Project files:
  - `mineconda.toml` (desired state)
  - `mineconda.lock` (resolved reproducible state)
- Optional S3 source:
  - `[sources.s3]` (experimental)
- Optional S3 cache backend:
  - `[cache.s3]` (experimental)

For full config details, use:

- `mineconda --help`
- `mineconda <command> --help`

## Development

Recommended local pipeline:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p mineconda-cli --release
MINECONDA_BIN="$(pwd)/target/release/mineconda" bash scripts/ci-smoke.sh
```

## Contributing

Issues and PRs are welcome. Please keep changes focused, include tests where appropriate, and ensure the validation pipeline passes before submission.

## License

MIT OR Apache-2.0
