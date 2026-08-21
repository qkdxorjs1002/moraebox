#!/bin/sh
set -eu

cargo +nightly miri setup
cargo +nightly miri test -p moraebox-core --lib
cargo +nightly miri test -p moraebox-protocol --lib round_trips_a_frame
