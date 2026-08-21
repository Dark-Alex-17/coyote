#!/usr/bin/env bash
set -e

# @describe Execute a git command. Strictly limited to a single git invocation: the command must start with 'git' and shell metacharacters (; & | < > ( ) $ `) are rejected outside single quotes — no pipes, chaining, redirection, or command substitution. Use git's own flags instead of pipes (e.g. 'git log -n 20' instead of piping to head). Output is never paginated and git will never prompt for input.
# @option --command! The git command to execute (e.g. "git status --short").

# @env LLM_OUTPUT=/dev/stdout The output path

# shellcheck disable=SC1090
source "$LLM_PROMPT_UTILS_FILE"

main() {
    # shellcheck disable=SC2154
    argc_command="$(jq -r '.command' <<< "$LLM_TOOL_RAW_JSON")"

    validate_command "$argc_command" "git"

    guard_operation "Execute git command: $argc_command"

    export GIT_PAGER=cat PAGER=cat GIT_TERMINAL_PROMPT=0
    export GIT_EDITOR=true GIT_SEQUENCE_EDITOR=true

    local script
    script="$(mktemp)"
    # shellcheck disable=SC2064
    trap "rm -f '$script'" EXIT
    printf '%s\n' "$argc_command" > "$script"
    bash -e -o pipefail "$script" >> "$LLM_OUTPUT"
}

die() {
    echo "$*" >&2
    exit 1
}

# Ensure the command is a single plain invocation of $2 with no shell escape
# hatches. Metacharacters are allowed inside single quotes (where bash treats
# them as literals) but rejected everywhere else, including $ and ` inside
# double quotes (expansion/substitution).
validate_command() {
    local cmd="$1" prog="$2"

    local first
    first="$(awk '{print $1}' <<< "$cmd")"
    if [[ "$first" != "$prog" ]]; then
        die "error: this tool only executes $prog commands; the command must start with '$prog' (got: '${first:-<empty>}')"
    fi

    local i c in_single=0 in_double=0 len=${#cmd}
    for (( i = 0; i < len; i++ )); do
        c="${cmd:i:1}"
        if (( in_single )); then
            [[ "$c" == "'" ]] && in_single=0
            continue
        fi
        if (( in_double )); then
            case "$c" in
                '\') i=$((i + 1)) ;;
                '"') in_double=0 ;;
                '$' | '`') die "error: '$c' is not allowed inside double quotes (expansion/substitution is blocked); use single quotes for literal text" ;;
            esac
            continue
        fi
        case "$c" in
            '\') i=$((i + 1)) ;;
            "'") in_single=1 ;;
            '"') in_double=1 ;;
            ';' | '&' | '|' | '<' | '>' | '(' | ')' | '$' | '`')
                die "error: shell metacharacter '$c' is not allowed; run a single $prog command with no pipes, chaining, redirection, or substitution (use $prog's own flags instead, and single quotes for literal text)"
                ;;
            $'\n')
                die "error: newlines are not allowed; run a single $prog command"
                ;;
        esac
    done
    if (( in_single || in_double )); then
        die "error: unbalanced quotes in command"
    fi
}
