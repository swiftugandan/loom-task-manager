# pre-commit gate

`.githooks/pre-commit` runs rustfmt, clippy `-D warnings`, `cargo test`, the plugin hook cases, and the agent-skill sync check against a **clean checkout of the index** in a temp directory, sharing the repo's `target/` via `CARGO_TARGET_DIR`.
So the thing verified is exactly the thing committed, and a broken staged file still fails even after you fix the worktree.

Two consequences worth knowing before you fight them:

- A red probe commit needs `git commit --no-verify`, since `cargo test` is part of the gate.
  That bypass is the documented path for recording failing acceptance tests.
  It also skips rustfmt and clippy, so the probe's own test file arrives unlinted and fails the next gated commit.
  Run `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings` over the probe file before the implementation commit.
- Tests that resolve paths through `env!("CARGO_MANIFEST_DIR")` bake the temp staging directory into the cached test binary.
  A later local `cargo test` reuses it and panics with `reading /private/var/folders/.../README.md: No such file or directory`.
  Fix: `touch` the affected test file (`tests/readme_coverage.rs`, `tests/skill_coverage.rs`) and re-run.
  The failure is a stale artifact, so treat a green re-run as the real result.
