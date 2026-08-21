# Safety and performance checks

The normal quality gate remains:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Property tests and the Loom lease/state models run with the workspace tests. The
standalone fuzz package keeps sanitizer-only dependencies out of the product
workspace:

```sh
cargo check --manifest-path fuzz/Cargo.toml --all-targets --locked
cargo +nightly fuzz run protocol_decode -- -runs=256
cargo +nightly fuzz run box_bundle -- -runs=256
cargo +nightly fuzz run image_reference -- -runs=256
scripts/miri-smoke.sh
```

`cargo-fuzz` and the nightly Miri component are required for the last four
commands. CI installs both in isolated jobs.

For a repeatable process-backend performance check, capture `morae --json
benchmark` output and compare it with the checked-in conservative ceiling:

```sh
cargo run -p moraebox-cli --locked -- --json benchmark \
  --backend process --iterations 10 --concurrency 2 \
  /usr/bin/printf ready > benchmark-report.json
scripts/check-benchmark-thresholds.py \
  benchmark-report.json tests/benchmark-thresholds.json
```

These process thresholds catch gross regressions without claiming VM-isolation
performance. Native cold/warm runs belong in the signed macOS smoke environment.
