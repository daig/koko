#!/bin/zsh
# Historical Ladybug-corpus sweep: run the Koko test runner over every external
# test_files directory and record per-directory output plus totals. This is optional
# post-v0 compatibility tooling, not a universal landing gate.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REFERENCE_ROOT=$(cd "${KOKO_REFERENCE_ROOT:-$ROOT/../ladybug}" 2>/dev/null && pwd) || {
  echo "Ladybug checkout not found; set KOKO_REFERENCE_ROOT" >&2
  exit 2
}
CORPUS=${KOKO_CORPUS_DIR:-$REFERENCE_ROOT/test/test_files}
OUT=${1:-$ROOT/target/goal-sweep}
BIN=$ROOT/target/release/koko-test
export KOKO_DATASET_DIR=${KOKO_DATASET_DIR:-$REFERENCE_ROOT/dataset}
export KOKO_ROOT_DIRECTORY=${KOKO_ROOT_DIRECTORY:-$REFERENCE_ROOT}
if [[ -z ${KOKO_EXPORT_DB_DIRECTORY:-} ]]; then
  export KOKO_EXPORT_DB_DIRECTORY="$OUT/exports/db"
  rm -rf "$OUT/exports"
  mkdir -p "$OUT/exports"
fi

if [[ ! -x $BIN ]]; then
  echo "building koko-test (release)..." >&2
  (cd "$ROOT" && cargo build --release -p koko-test-runner) || exit 2
fi
TIMEOUT_BIN=$(command -v timeout || true)
mkdir -p "$OUT"; rm -f "$OUT"/*.txt

run_one() { # $1 = path, $2 = outfile
  if [[ -n $TIMEOUT_BIN ]]; then "$TIMEOUT_BIN" 900 "$BIN" "$1" > "$2" 2>&1
  else "$BIN" "$1" > "$2" 2>&1; fi
  local rc=$?
  [[ $rc -eq 124 ]] && echo "TIMEOUT(900s)" >> "$2"
  echo "$rc"
}

cd "$CORPUS"
for d in */; do
  d=${d%/}
  rc=$(run_one "$CORPUS/$d" "$OUT/$d.txt")
  echo "$d rc=$rc"
done
for f in "$CORPUS"/*.test(N); do
  n=$(basename "$f" .test)
  rc=$(run_one "$f" "$OUT/$n.txt")
  echo "$n rc=$rc"
done

awk -F'[ ,]+' '/passed.*skipped.*failed/{p+=$1; s+=$3; f+=$5} \
  END {printf "TOTALS: %d passed, %d skipped, %d failed\n", p, s, f}' "$OUT"/*.txt
PANICS=$(grep -l "panicked" "$OUT"/*.txt 2>/dev/null | wc -l | tr -d ' ')
TIMEOUTS=$(grep -l "TIMEOUT(900s)" "$OUT"/*.txt 2>/dev/null | wc -l | tr -d ' ')
echo "PANIC_FILES: $PANICS   TIMEOUT_FILES: $TIMEOUTS   (outputs: $OUT)"
grep -h "^FAIL" "$OUT"/*.txt > "$OUT/_all_fails.txt" 2>/dev/null
echo "FAIL_LINES: $(wc -l < "$OUT/_all_fails.txt" | tr -d ' ') (see $OUT/_all_fails.txt)"
[[ $PANICS -eq 0 ]] || exit 1
