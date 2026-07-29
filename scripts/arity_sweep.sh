#!/bin/zsh
# Robustness sweep: call every scalar-function name with 0 and 1 args in a fresh
# process; count panics vs clean errors vs accepts. Gate: panics=0 (docs/GOAL.md §3.5).
# Usage: scripts/arity_sweep.sh [-v]   (-v lists each PANIC probe)
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLI=$ROOT/target/release/examples/koko_cli
VERBOSE=${1:-}
if [[ ! -x $CLI ]]; then
  echo "building koko_cli example (release)..." >&2
  (cd "$ROOT" && cargo build --release --example koko_cli) || exit 2
fi
NAMES=$(grep -oE '^\s+"[a-z_0-9]+"( \| "[a-z_0-9]+")*' \
        "$ROOT/crates/koko-function/src/scalarfn.rs" | grep -oE '"[a-z_0-9]+"' | tr -d '"' | sort -u)
panics=0; errors=0; oks=0
for f in ${(f)NAMES}; do
  for call in "RETURN $f()" "RETURN $f('x')"; do
    out=$(echo "$call;" | "$CLI" 2>&1)
    if [[ "$out" == *panicked* ]]; then
      panics=$((panics+1))
      [[ "$VERBOSE" == "-v" ]] && echo "PANIC: $call"
    elif [[ "$out" == *Error:* ]]; then errors=$((errors+1))
    else oks=$((oks+1)); fi
  done
done
echo "panics=$panics errors=$errors oks=$oks (names: $(echo "$NAMES" | wc -l | tr -d ' '))"
[[ $panics -eq 0 ]] || exit 1
