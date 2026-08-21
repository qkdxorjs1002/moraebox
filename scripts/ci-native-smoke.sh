#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repository_root"

native_status=failed
native_detail="the smoke script exited before native validation completed"

report_result() {
    exit_code=$?
    if [ "$native_status" = "failed" ]; then
        native_detail="$native_detail (exit code $exit_code)"
    fi
    line="Native signed smoke: $native_status — $native_detail"
    printf '%s\n' "$line"
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
        printf '### %s\n' "$line" >> "$GITHUB_STEP_SUMMARY"
    fi
}

skip_smoke() {
    native_status=skipped
    native_detail=$1
    exit 0
}

append_missing() {
    if [ -z "$missing" ]; then
        missing=$1
    else
        missing="$missing, $1"
    fi
}

formula_prefix() {
    brew --prefix "$1" 2>/dev/null || true
}

trap report_result EXIT

[ "$(uname -s)" = "Darwin" ] || skip_smoke "requires macOS"
[ "$(uname -m)" = "arm64" ] || skip_smoke "requires Apple Silicon arm64"
command -v cargo >/dev/null 2>&1 || skip_smoke "Rust cargo is unavailable"
command -v codesign >/dev/null 2>&1 || skip_smoke "Apple codesign is unavailable"
command -v brew >/dev/null 2>&1 || skip_smoke "Homebrew is unavailable"

libkrun_prefix=$(formula_prefix libkrun)
libkrunfw_prefix=$(formula_prefix libkrunfw)
gvproxy_prefix=$(formula_prefix gvproxy)
e2fsprogs_prefix=$(formula_prefix e2fsprogs)

missing=""
[ -f "$libkrun_prefix/lib/libkrun.dylib" ] || append_missing "libkrun.dylib"
[ -f "$libkrunfw_prefix/lib/libkrunfw.dylib" ] || append_missing "libkrunfw.dylib"
[ -x "$gvproxy_prefix/bin/gvproxy" ] || append_missing "gvproxy"
[ -x "$e2fsprogs_prefix/sbin/mke2fs" ] || append_missing "mke2fs"
[ -x "$e2fsprogs_prefix/sbin/e2fsck" ] || append_missing "e2fsck"

if [ -n "$missing" ]; then
    setup_outcome=${MORAE_NATIVE_DEPENDENCY_SETUP:-not-reported}
    skip_smoke "missing native capability: $missing; dependency setup outcome: $setup_outcome"
fi

export MORAE_LIBKRUN_PATH="$libkrun_prefix/lib/libkrun.dylib"
export MORAE_LIBKRUNFW_PATH="$libkrunfw_prefix/lib/libkrunfw.dylib"
export MORAE_GVPROXY_PATH="$gvproxy_prefix/bin/gvproxy"
export MORAE_MKE2FS="$e2fsprogs_prefix/sbin/mke2fs"
export MORAE_E2FSCK="$e2fsprogs_prefix/sbin/e2fsck"
export MORAE_LIB_DIR="$libkrun_prefix/lib:$libkrunfw_prefix/lib"

cache_dir=${MORAE_NATIVE_E2E_CACHE_DIR:-${RUNNER_TEMP:-/private/tmp}/moraebox-native-cache}
image=${MORAE_NATIVE_E2E_IMAGE:-python:3.12}
mkdir -p "$cache_dir"

native_detail="workspace build or OCI image preparation failed"
cargo build --workspace --locked
target/debug/morae image pull "$image" --cache-dir "$cache_dir"

native_detail="doctor or real-backend native egress suite failed"
MORAE_NATIVE_E2E_CACHE_DIR="$cache_dir" \
    scripts/native-egress-e2e.sh

native_status=passed
native_detail="executed doctor and the real-backend signed egress suite on Apple Silicon"
