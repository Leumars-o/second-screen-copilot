#!/usr/bin/env bash
# Gate script (Build Sheet §3.6). Usage:
#   scripts/check.sh                      # everything
#   scripts/check.sh crates/ssc-screen    # one or more owned paths (Rust crates and/or TS packages)
# Always runs the contract conformance tests and the frozen-contract check.
set -euo pipefail
cd "$(dirname "$0")/.."

RUST_PKGS=()
TS_FILTERS=()
for p in "$@"; do
  p="${p%/}"
  case "$p" in
    crates/*)    RUST_PKGS+=("-p" "$(basename "$p")") ;;
    apps/agent)  RUST_PKGS+=("-p" "ssc-agent") ;;
    apps/phone|packages/*|services/relay) TS_FILTERS+=("--filter" "./$p") ;;
    bench|bench/*) RUST_PKGS+=("--workspace") ;;  # W2-BENCH may add crates or scripts under bench/
    *) echo "check.sh: unknown path '$p'" >&2; exit 2 ;;
  esac
done
ALL=$([[ $# -eq 0 ]] && echo 1 || echo 0)

step() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

step "frozen contracts"
# Newest contracts-v* tag reachable from HEAD (contracts-v1, contracts-v1.1, ...).
CTAG=$(git describe --tags --match 'contracts-v*' --abbrev=0 2>/dev/null || true)
if [[ -n "$CTAG" && -z "${SSC_ALLOW_CONTRACT_CHANGES:-}" ]]; then
  changed=$(git diff --name-only "$CTAG" -- contracts crates/ssc-core | grep -v '^contracts/CHANGE_REQUESTS.md$' || true)
  if [[ -n "$changed" ]]; then
    echo "Frozen files changed since $CTAG (file a change request instead):"; echo "$changed"; exit 1
  fi
  echo "ok (unchanged since $CTAG)"
else
  echo "skipped (no contracts-v* tag yet, or SSC_ALLOW_CONTRACT_CHANGES set by the lead)"
fi

step "contract conformance"
cargo test -q -p ssc-core --test conformance --test db_schema

if [[ $ALL -eq 1 || ${#RUST_PKGS[@]} -gt 0 ]]; then
  if [[ $ALL -eq 1 ]]; then SCOPE=(--workspace); else SCOPE=("${RUST_PKGS[@]}"); fi
  step "rustfmt"
  cargo fmt --all -- --check
  step "clippy ${SCOPE[*]}"
  cargo clippy -q "${SCOPE[@]}" --all-targets -- -D warnings
  step "cargo test ${SCOPE[*]}"
  cargo test -q "${SCOPE[@]}"
fi

if [[ $ALL -eq 1 || ${#TS_FILTERS[@]} -gt 0 ]]; then
  [[ -d node_modules ]] || pnpm install --frozen-lockfile
  # ${arr[@]+...} keeps bash 3.2 (macOS) happy with empty arrays under `set -u`.
  step "typecheck ${TS_FILTERS[*]+${TS_FILTERS[*]}}"
  pnpm -r ${TS_FILTERS[@]+"${TS_FILTERS[@]}"} --if-present typecheck
  step "ts tests ${TS_FILTERS[*]+${TS_FILTERS[*]}}"
  pnpm -r ${TS_FILTERS[@]+"${TS_FILTERS[@]}"} --if-present test
fi

printf '\n\033[32mcheck.sh: all gates passed\033[0m\n'
