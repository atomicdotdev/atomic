#!/usr/bin/env bash
# Native-only Python checks report each exercised assertion back to helpers.sh.
run_native_checks() {
    local script="$1" evidence private rc=0 line failures_before="$TESTS_FAILED" passed=0
    shift
    evidence="$(mktemp "${TMPDIR:-/tmp}/atomic-native-checks-XXXXXX")"
    _HARNESS_TMPDIRS+=("$evidence")
    private="$(mktemp -d "${TMPDIR:-/tmp}/atomic-native-home-XXXXXX")"
    _HARNESS_TMPDIRS+=("$private")
    HOME="$private" ATOMIC_HOME="$private/.atomic" ATOMIC_CONFIG_DIR="$private/config" ATOMIC_SERVICE=local PYTHONDONTWRITEBYTECODE=1 \
        python3 "$HARNESS_DIR/$script" "$ATOMIC_BIN" "$@" > "$evidence" 2>&1 || rc=$?
    while IFS= read -r line; do
        case "$line" in
            PASS$'\t'*) _pass "${line#*$'\t'}"; passed=$((passed + 1)) ;;
            FAIL$'\t'*) _fail "${line#*$'\t'}" "native workflow assertion failed" ;;
            *) printf '  %s\n' "$line" ;;
        esac
    done < "$evidence"
    if [[ "$rc" != 0 && "$TESTS_FAILED" == "$failures_before" ]]; then
        _fail "native driver completes" "exit $rc; remaining assertions were not exercised"
    elif [[ "$rc" == 0 && "$passed" == 0 ]]; then
        _fail "native driver exercises assertions" "driver reported no passes"
    fi
}
