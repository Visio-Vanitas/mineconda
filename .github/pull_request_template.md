## Summary

- Describe the user-facing change.

## Validation

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `cargo build -p mineconda-cli --release`
- [ ] `MINECONDA_BIN="$(pwd)/target/release/mineconda" bash scripts/ci-smoke.sh`

## Notes

- Mention compatibility risks, follow-up work, or areas that need reviewer attention.
