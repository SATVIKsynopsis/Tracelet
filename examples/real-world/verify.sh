#!/usr/bin/env bash
# Positive/negative validation for every real-world example.
#
# For each example directory:
#   1. run the suite against the original collector.yaml   -> must exit 0
#   2. for each regressions/<name>.patch:
#        apply it to collector.yaml
#        run the suite                                      -> must exit 1, and
#          the assertion in regressions/<name>.expect must be reported as failed
#        revert the patch (collector.yaml checksum must match the original)
#        run the suite again                                -> must exit 0
#
# Usage: ./verify.sh [example-dir...]   (default: every NN-* directory)
# OTEL_TEST=/path/to/otel-test overrides the binary (default: `otel-test` on
# PATH, else ../../target/release/otel-test).
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
bin=${OTEL_TEST:-$(command -v otel-test || echo "$here/../../target/release/otel-test")}
[[ -x $bin ]] || { echo "otel-test binary not found (set OTEL_TEST)"; exit 2; }
[[ $bin == /* ]] || bin="$PWD/$bin"

cd "$here"
dirs=("$@")
[[ ${#dirs[@]} -gt 0 ]] || dirs=([0-9][0-9]-*/)
failures=0
log=$(mktemp)
trap 'rm -f "$log"' EXIT

checksum() { shasum -a 256 "$1" | cut -d' ' -f1; }

run() { # run <dir> -> sets $code, output in $log
    "$bin" --color never test -f "$1/otel-test.yaml" >"$log" 2>&1
    code=$?
}

fail() {
    echo "    FAIL: $*"
    sed 's/^/      | /' "$log"
    failures=$((failures + 1))
}

for d in "${dirs[@]}"; do
    d=${d%/}
    echo "== $d"
    original=$(checksum "$d/collector.yaml")

    run "$d"
    if [[ $code -eq 0 ]]; then
        echo "  original config: PASS ($(grep -E '^[0-9]+ passed' "$log"); $(grep '^Time' "$log"))"
    else
        fail "original config exited $code, expected 0"
        continue
    fi

    for p in "$d"/regressions/*.patch; do
        [[ -e $p ]] || { fail "no regressions/*.patch"; break; }
        name=$(basename "$p" .patch)
        if ! patch -s -p1 -d "$d" <"$p"; then
            fail "$name: patch did not apply"
            continue
        fi

        run "$d"
        # "test name :: assertion" for every failed assertion.
        failed=$(awk '/^✗ /{t=$0; sub(/^✗ /, "", t)}
                     /^    ✗ /{a=$0; sub(/^    ✗ /, "", a); print t " :: " a}' "$log")
        if [[ $code -ne 1 ]]; then
            fail "$name: exited $code with the regression applied, expected 1"
        elif missing=$(grep -vxF -f <(printf '%s\n' "$failed") "${p%.patch}.expect"); then
            fail "$name: expected failure(s) not reported: $missing"
        else
            echo "  regression $name: FAIL as expected ($(grep -E '^[0-9]+ passed' "$log"))"
            while IFS= read -r a; do echo "      ✗ $a"; done <<<"$failed"
        fi

        patch -s -R -p1 -d "$d" <"$p"
        if [[ $(checksum "$d/collector.yaml") != "$original" ]]; then
            fail "$name: collector.yaml not restored"
            break
        fi
        run "$d"
        if [[ $code -eq 0 ]]; then
            echo "  restored:          PASS"
        else
            fail "$name: restored config exited $code, expected 0"
        fi
    done
done

echo
if [[ $failures -eq 0 ]]; then
    echo "all examples verified"
else
    echo "$failures check(s) failed"
    exit 1
fi
