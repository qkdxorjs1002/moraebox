#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repository_root"

if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
    echo "native egress E2E requires Apple Silicon macOS" >&2
    exit 2
fi

cargo test -p moraebox-vmm-helper
cargo build --workspace

codesign --force --sign - \
    --entitlements assets/moraebox-vmm.entitlements \
    target/debug/morae-vmm-helper
codesign --verify --strict target/debug/morae-vmm-helper

target/debug/morae doctor --json --strict
MORAE_NATIVE_E2E=1 cargo test \
    -p moraebox-cli \
    --test native_egress \
    -- \
    --ignored \
    --nocapture \
    --test-threads=1
