# Reginux fuzz targets

This directory is intentionally outside the main Cargo workspace. It exercises
the public structured readers and writers with arbitrary UTF-8-lossy input,
including malformed TOML, INI, KDL and line-oriented configuration.

Install `cargo-fuzz` once, then run the target from the repository root:

```bash
cargo install cargo-fuzz --locked
cargo fuzz run structured -- -max_total_time=300
```

Crash artifacts are written below `fuzz/artifacts/` and must be minimized and
turned into a regression test before a release. The scheduled GitHub workflow
runs this target for five minutes; a release is not considered complete when a
new fuzz crash is unresolved.
