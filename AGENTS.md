# Agent instructions

## CI checks before committing

Before creating a commit, read `.github/workflows/ci.yml` and run the CI checks
locally. Keep the commands aligned with the workflow as it changes. The current
checks are:

```sh
cargo fmt --all -- --check
cargo test --locked
cargo build --locked
python3 scripts/smoke_tui.py
cargo clippy --locked --all-targets -- -D warnings
```

Build before running the TUI smoke test so it exercises the current source.
Fix failures and rerun the affected checks before committing. Report any checks
that could not run locally, including Rust 1.86 compatibility or platform-specific
builds; do not claim those checks passed.

If the commit is pushed, check the GitHub Actions run for that commit and report
its status. Investigate and fix CI failures caused by the changes.
