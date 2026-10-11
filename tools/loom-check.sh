#!/usr/bin/env bash
# =============================================================================
# 🧵 loom-check.sh — exhaustive concurrency verification for sniper-telemetry
# =============================================================================
#
# WHY THIS IS A SCRIPT AND NOT A CARGO ALIAS
#
# loom only works if `src/` is compiled with `--cfg loom`, because that is what
# makes `histogram.rs` and `counter.rs` import loom's atomics instead of std's.
# Without it, loom has nothing to instrument: the command compiles, every test
# passes, and nothing was verified. That is the worst failure mode a
# concurrency check can have.
#
# RUSTFLAGS is an environment variable, and a cargo alias cannot set one. The
# array-alias form that could --
#
#     tloom = ["sh", "-c", "RUSTFLAGS='--cfg loom' cargo test ..."]
#
# -- is resolved by cargo 1.99 as a cargo-subcommand lookup and fails with
# "no such command: sh". Hence this script.
#
# WHY RELEASE
#
# Two separate reasons, both load-bearing:
#
#   1. Under a debug build every atomic access carries bookkeeping, which
#      multiplies the state space loom has to enumerate.
#   2. `cargo test --release` runs the `bench` profile, and this workspace sets
#      `panic = "abort"` on `release`. Cargo forces `panic = "unwind"` onto
#      test targets, so unwinding still works -- but relying on that is exactly
#      the kind of assumption that should be verified by the suite rather than
#      remembered, which is what test_profile_invariants.rs is for.
#
# THE LIMITS BELOW ARE BACKSTOPS, NOT A BUDGET
#
# Every loom test in this repository completes its exhaustive search in about a
# second. If LOOM_MAX_PERMUTATIONS is ever hit, a test has grown too complex --
# the fix is to simplify the test, never to raise the cap until it goes quiet.
# A concurrency check that silently explores a fraction of the state space and
# reports success is worse than no check, because it is believed.
#
# =============================================================================

set -euo pipefail

# --- resolve the repository root ------------------------------------------------
# tools/ -> parent. Resolved from this script's own location so the check runs
# from any working directory, which is what a CI job and a human both do.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# --- find cargo -----------------------------------------------------------------
# rustup installs into ~/.cargo/bin, which is not always on a non-interactive
# shell's PATH -- including this script's. Prepending it costs nothing when the
# binary is already there and saves a confusing "cargo: command not found".
if ! command -v cargo >/dev/null 2>&1; then
    if [ -x "$HOME/.cargo/bin/cargo" ]; then
        export PATH="$HOME/.cargo/bin:$PATH"
    else
        echo "❌ cargo not found on PATH and not at ~/.cargo/bin/cargo" >&2
        echo "   Install Rust from https://rustup.rs and re-run." >&2
        exit 127
    fi
fi

# --- the flag that makes this file meaningful ------------------------------------
export RUSTFLAGS="--cfg loom"

# loom 0.7 configures these through the environment only; Builder has no
# setters for them. Values chosen from the observed run time of roughly one
# second, with roughly twenty times that as headroom for slower CI hardware.
export LOOM_MAX_PERMUTATIONS="${LOOM_MAX_PERMUTATIONS:-20000}"
export LOOM_MAX_BRANCHES="${LOOM_MAX_BRANCHES:-10000}"

echo "🧵 loom: RUSTFLAGS=$RUSTFLAGS  permutations<=$LOOM_MAX_PERMUTATIONS"
echo "   repo: $REPO_ROOT"

# --- run ------------------------------------------------------------------------
# Both flags are load-bearing and are spelled out here rather than hidden in an
# alias, so that anyone reading CI logs can see exactly what was verified.
set +e
cargo test -p sniper-telemetry --test loom_concurrency --release
STATUS=$?
set -e

if [ "$STATUS" -ne 0 ]; then
    echo ""
    echo "❌ loom found a failing interleaving (or failed to build)." >&2
    echo "" >&2
    echo "   If the build failed with 'cannot find loom', the cfg(loom)" >&2
    echo "   dependency is missing from crates/telemetry/Cargo.toml." >&2
    echo "   If a test failed, the minimal failing schedule is in the output" >&2
    echo "   above -- loom prints the exact interleaving that broke it." >&2
    exit "$STATUS"
fi

echo ""
echo "✅ Every interleaving loom explored agrees with the always-on layer."