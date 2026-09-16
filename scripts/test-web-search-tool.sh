#!/usr/bin/env bash
# Regression harness for assets/functions/tools/web_search_coyote.sh.
#
# Runs the tool against a fake `coyote` executable on PATH that records every
# invocation (model + the search-patch env vars it received) and returns
# scripted per-call exit codes, pinning:
#   A) openai attempt chain order: the chat-completions mechanism (search-preview
#      model) runs first, the Responses mechanism (responses-capable model)
#      runs second, and the run fails only when both attempts fail;
#   B) both openai patch env vars are exported with the expected values so the
#      client's resolved wire (chat vs responses) picks the matching one;
#   C) model mapping between the two attempts (search-preview <-> base models);
#   D) non-openai providers are untouched: single invocation, their own patch,
#      openai patch keys cleared;
#   E) the WEB_SEARCH_MODEL fallback and WEB_SEARCH_USE_CURRENT_MODEL
#      semantics are unchanged.
#
# No network or credentials required. Usage: scripts/test-web-search-tool.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TOOL="$REPO_ROOT/assets/functions/tools/web_search_coyote.sh"

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT
export STUB_DIR="$WORK_DIR"

# Expected patch values (must stay in sync with the tool script).
EXPECTED_OPENAI_CHAT_PATCH='{"gpt-4o.*search-preview.*":{"body":{"web_search_options":{}}}}'
EXPECTED_OPENAI_RESPONSES_PATCH='{"(gpt-4o|gpt-4\\.1|gpt-5|o3|o4).*":{"body":{"tools":[{"type":"web_search"}]}}}'
EXPECTED_CLAUDE_PATCH='{".*":{"body":{"tools":[{"type":"web_search_20250305","name":"web_search","max_uses":5}]}}}'

# Fake coyote: logs "call=N model=M chat=... responses=... own=..." per
# invocation and exits with the N-th code from COYOTE_STUB_RESULTS.
cat >"$WORK_DIR/coyote" <<'STUB'
#!/usr/bin/env bash
n=$(( $(cat "$STUB_DIR/count" 2>/dev/null || echo 0) + 1 ))
echo "$n" >"$STUB_DIR/count"
model=""
[[ "${1:-}" == "-m" ]] && model="${2:-}"
{
    printf 'call=%s model=%s\n' "$n" "$model"
    printf 'call=%s chat_patch=%s\n' "$n" "${COYOTE_PATCH_OPENAI_CHAT_COMPLETIONS-<unset>}"
    printf 'call=%s responses_patch=%s\n' "$n" "${COYOTE_PATCH_OPENAI_RESPONSES-<unset>}"
    printf 'call=%s gemini_patch=%s\n' "$n" "${COYOTE_PATCH_GEMINI_CHAT_COMPLETIONS-<unset>}"
    printf 'call=%s claude_patch=%s\n' "$n" "${COYOTE_PATCH_CLAUDE_CHAT_COMPLETIONS-<unset>}"
} >>"$STUB_DIR/log"
read -r -a results <<<"${COYOTE_STUB_RESULTS:-}"
rc="${results[$((n - 1))]:-0}"
[[ "$rc" == "0" ]] && echo "stub-output-$n"
exit "$rc"
STUB
chmod +x "$WORK_DIR/coyote"

FAILED=0
CASE=""

fail() {
    echo "FAIL [$CASE]: $*" >&2
    echo "--- invocation log ---" >&2
    cat "$STUB_DIR/log" 2>/dev/null >&2 || true
    echo "----------------------" >&2
    FAILED=1
}

# run_tool <results> [VAR=value ...]
# Sources the tool with the argc runtime emulated (argc_query/LLM_OUTPUT set)
# and runs main(). Sets TOOL_RC to the tool's exit code.
run_tool() {
    local results="$1"
    shift
    rm -f "$STUB_DIR/log" "$STUB_DIR/count"
    : >"$STUB_DIR/out"
    TOOL_RC=0
    (
        export PATH="$STUB_DIR:$PATH"
        export COYOTE_STUB_RESULTS="$results"
        export argc_query="test query"
        export LLM_OUTPUT="$STUB_DIR/out"
        unset WEB_SEARCH_MODEL COYOTE_CURRENT_MODEL WEB_SEARCH_USE_CURRENT_MODEL
        local kv
        for kv in "$@"; do
            # shellcheck disable=SC2163
            export "${kv?}"
        done
        # shellcheck disable=SC1090
        source "$TOOL"
        main
    ) || TOOL_RC=$?
}

assert_calls() {
    local expected="$1" actual
    actual="$(cat "$STUB_DIR/count" 2>/dev/null || echo 0)"
    [[ "$actual" == "$expected" ]] || fail "expected $expected coyote invocation(s), got $actual"
}

assert_log() {
    local line="$1"
    grep -qF -- "$line" "$STUB_DIR/log" || fail "missing log line: $line"
}

assert_rc() {
    local expected="$1"
    if [[ "$expected" == "0" ]]; then
        [[ "$TOOL_RC" -eq 0 ]] || fail "expected success, got exit code $TOOL_RC"
    else
        [[ "$TOOL_RC" -ne 0 ]] || fail "expected failure, tool exited 0"
    fi
}

# --- A/B: stock success — chat mechanism runs first with both patches set ---
CASE="openai current model, first attempt succeeds"
run_tool "0" COYOTE_CURRENT_MODEL=openai:gpt-4o
assert_rc 0
assert_calls 1
assert_log "call=1 model=openai:gpt-4o-search-preview"
assert_log "call=1 chat_patch=$EXPECTED_OPENAI_CHAT_PATCH"
assert_log "call=1 responses_patch=$EXPECTED_OPENAI_RESPONSES_PATCH"

CASE="search-preview current model kept as-is on the chat attempt (pinned wire_api: chat behavior)"
run_tool "0" COYOTE_CURRENT_MODEL=openai:gpt-4o-search-preview
assert_rc 0
assert_calls 1
assert_log "call=1 model=openai:gpt-4o-search-preview"
assert_log "call=1 chat_patch=$EXPECTED_OPENAI_CHAT_PATCH"

# --- A/C: chat attempt fails -> Responses attempt on a responses-capable model ---
CASE="openai chain: chat fails, Responses succeeds (current model responses-capable)"
run_tool "1 0" COYOTE_CURRENT_MODEL=openai:gpt-4o
assert_rc 0
assert_calls 2
assert_log "call=1 model=openai:gpt-4o-search-preview"
assert_log "call=2 model=openai:gpt-4o"
assert_log "call=2 responses_patch=$EXPECTED_OPENAI_RESPONSES_PATCH"

CASE="openai chain: search-preview model maps to its base model on the Responses attempt"
run_tool "1 0" COYOTE_CURRENT_MODEL=openai:gpt-4o-search-preview
assert_rc 0
assert_calls 2
assert_log "call=1 model=openai:gpt-4o-search-preview"
assert_log "call=2 model=openai:gpt-4o"

CASE="openai chain: mini variants map both ways"
run_tool "1 0" COYOTE_CURRENT_MODEL=openai:gpt-4o-mini-search-preview
assert_rc 0
assert_calls 2
assert_log "call=1 model=openai:gpt-4o-mini-search-preview"
assert_log "call=2 model=openai:gpt-4o-mini"

CASE="openai chain: gpt-5 current model runs the Responses attempt on itself"
run_tool "1 0" COYOTE_CURRENT_MODEL=openai:gpt-5
assert_rc 0
assert_calls 2
assert_log "call=1 model=openai:gpt-4o-search-preview"
assert_log "call=2 model=openai:gpt-5"

# --- A/E: both openai attempts fail -> existing WEB_SEARCH_MODEL fallback ---
CASE="openai chain exhausted falls back to WEB_SEARCH_MODEL with openai patches cleared"
run_tool "1 1 0" COYOTE_CURRENT_MODEL=openai:gpt-4o
assert_rc 0
assert_calls 3
assert_log "call=3 model=gemini:gemini-2.5-flash"
assert_log "call=3 chat_patch=<unset>"
assert_log "call=3 responses_patch=<unset>"
assert_log 'call=3 gemini_patch={".*":{"body":{"tools":[{"google_search":{}}]}}}'

# --- A: hard failure when both attempts of the last resort fail ---
CASE="openai fallback model: both attempts fail -> tool fails"
run_tool "1 1" WEB_SEARCH_MODEL=openai:gpt-4o
assert_rc 1
assert_calls 2
assert_log "call=1 model=openai:gpt-4o-search-preview"
assert_log "call=2 model=openai:gpt-4o"

# --- D: non-openai providers untouched ---
CASE="claude current model: single invocation, own patch, openai keys cleared"
run_tool "0" COYOTE_CURRENT_MODEL=claude:claude-sonnet-4-20250514 \
    COYOTE_PATCH_OPENAI_CHAT_COMPLETIONS=stale COYOTE_PATCH_OPENAI_RESPONSES=stale
assert_rc 0
assert_calls 1
assert_log "call=1 model=claude:claude-sonnet-4-20250514"
assert_log "call=1 claude_patch=$EXPECTED_CLAUDE_PATCH"
assert_log "call=1 chat_patch=<unset>"
assert_log "call=1 responses_patch=<unset>"

CASE="default fallback (gemini): single invocation"
run_tool "0"
assert_rc 0
assert_calls 1
assert_log "call=1 model=gemini:gemini-2.5-flash"
assert_log 'call=1 gemini_patch={".*":{"body":{"tools":[{"google_search":{}}]}}}'

# --- E: unsupported / opted-out current models skip straight to fallback ---
CASE="unsupported openai model (o1) goes straight to fallback"
run_tool "0" COYOTE_CURRENT_MODEL=openai:o1-mini
assert_rc 0
assert_calls 1
assert_log "call=1 model=gemini:gemini-2.5-flash"

CASE="WEB_SEARCH_USE_CURRENT_MODEL=false ignores the current model"
run_tool "0" COYOTE_CURRENT_MODEL=openai:gpt-4o WEB_SEARCH_USE_CURRENT_MODEL=false
assert_rc 0
assert_calls 1
assert_log "call=1 model=gemini:gemini-2.5-flash"

# --- E: current-model failure still falls back (chain counts as one failure) ---
CASE="openai current chain exhausted, non-openai fallback succeeds, output appended"
run_tool "1 1 0" COYOTE_CURRENT_MODEL=openai:gpt-4.1 WEB_SEARCH_MODEL=gemini:gemini-2.5-pro
assert_rc 0
assert_calls 3
assert_log "call=1 model=openai:gpt-4o-search-preview"
assert_log "call=2 model=openai:gpt-4.1"
assert_log "call=3 model=gemini:gemini-2.5-pro"
grep -qF "stub-output-3" "$STUB_DIR/out" || fail "fallback output missing from LLM_OUTPUT"

if [[ "$FAILED" -ne 0 ]]; then
    echo "web_search_coyote harness: FAILED" >&2
    exit 1
fi
echo "web_search_coyote harness: all cases passed"
