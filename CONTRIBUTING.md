# Contributing to mineconda

Thanks for contributing.

## Before You Start

- Open a bug report or feature request if the change is large, user-visible, or changes CLI behavior.
- Keep pull requests focused. Avoid mixing refactors, new features, and unrelated cleanup.
- Update documentation when command behavior, config shape, or output changes.

## Development Workflow

Recommended validation pipeline:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p mineconda-cli --release
MINECONDA_BIN="$(pwd)/target/release/mineconda" bash scripts/ci-smoke.sh
```

If you touch experimental S3 behavior, keep it clearly marked as experimental and validate it separately from the stable baseline.

## Pull Requests

- Explain the user-facing problem and the chosen approach.
- Include tests for new behavior or regression coverage for bug fixes.
- Mention any compatibility risks, especially for manifest, lockfile, import/export, or CLI output changes.
- Keep CI green before requesting review.

## Style Notes

- Prefer small, reviewable commits.
- Preserve existing CLI behavior unless the change explicitly updates it.
- Do not add environment-specific secrets, hostnames, credentials, or private infrastructure details to the repository.
