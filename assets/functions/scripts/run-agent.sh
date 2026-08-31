#!/usr/bin/env bash

# Usage: ./{agent_name}.sh <agent-func> <agent-data>

set -e

main() {
    self_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    agent_dir="$(cd "$self_dir/.." && pwd)"
    root_dir="$(resolve_dir "{root_dir_env}" "$self_dir/{root_dir_rel}")"
    functions_dir="$(resolve_dir "{functions_dir_env}" "$self_dir/{functions_dir_rel}")"
    parse_argv "$@"
    setup_env
    tools_path="$agent_dir/tools.sh"
    run
}

# Resolve a directory at run time: prefer the override env var ($1) when set,
# otherwise fall back to the default path ($2) derived from this script's own
# location, so the shim keeps working when the config dir moves or is shared
# across environments with different home directories.
resolve_dir() {
    local override
    override="$(printenv "$1" 2>/dev/null || true)"
    if [[ -n "$override" ]]; then
        echo "$override"
    else
        (cd "$2" 2>/dev/null && pwd) || echo "$2"
    fi
}

parse_argv() {
		agent_func="$1"
    if [[ -n "$LLM_TOOL_DATA_FILE" ]] && [[ -f "$LLM_TOOL_DATA_FILE" ]]; then
      	agent_data="$(cat "$LLM_TOOL_DATA_FILE")"
    else
				agent_data="$2"
    fi
    if [[ -z "$agent_data" ]] || [[ -z "$agent_func" ]]; then
        die "usage: ./{agent_name}.sh <agent-func> <agent-data>"
    fi
}

setup_env() {
    load_env "$root_dir/.env"
    export LLM_ROOT_DIR="$root_dir"
    export LLM_AGENT_NAME="{agent_name}"
    export LLM_AGENT_FUNC="$agent_func"
    export LLM_AGENT_ROOT_DIR="$agent_dir"
    export LLM_AGENT_CACHE_DIR="$LLM_ROOT_DIR/cache/{agent_name}"
    export LLM_PROMPT_UTILS_FILE="$functions_dir/utils/prompt-utils.sh"
    export LLM_AGENT_RAW_JSON="$agent_data"
}

load_env() {
    local env_file="$1" env_vars
    if [[ -f "$env_file" ]]; then
        while IFS='=' read -r key value; do
            if [[ "$key" == $'#'* ]] || [[ -z "$key" ]]; then
                continue
            fi

            if [[ -z "${!key+x}" ]]; then
                env_vars="$env_vars $key=$value"
            fi
        done < <(cat "$env_file"; echo "")

        if [[ -n "$env_vars" ]]; then
            eval "export $env_vars"
        fi
    fi
}

run() {
    if [[ -z "$agent_data" ]]; then
        die "error: no JSON data"
    fi

    if [[ ! -f "$tools_path" ]]; then
        die "error: agent tools script not found: $tools_path"
    fi

    if [[ "$OS" == "Windows_NT" ]]; then
        set -o igncr
        tools_path="$(cygpath -w "$tools_path")"
    fi

    jq_script="$(cat <<-'EOF'
def escape_shell_word:
  tostring
  | gsub("'"; "'\"'\"'")
  | gsub("\n"; "'$'\\n''")
  | "'\(.)'";
def to_args:
    to_entries | .[] |
    (.key | split("_") | join("-")) as $key |
    if .value | type == "array" then
        .value | .[] | "--\($key)=\(. | escape_shell_word)"
    elif .value | type == "boolean" then
        if .value then "--\($key)" else "" end
    else
        "--\($key)=\(.value | escape_shell_word)"
    end;
[ to_args ] | join(" ")
EOF
)"
    args="$(echo "$agent_data" | jq -r "$jq_script" 2>/dev/null)" || {
        die "error: invalid JSON data"
    }

    if [[ -z "$LLM_OUTPUT" ]]; then
        is_temp_llm_output=1
        # shellcheck disable=SC2155
        export LLM_OUTPUT="$(mktemp)"
    fi

    eval "'$tools_path' '$agent_func' $args"

    if [[ "$is_temp_llm_output" -eq 1 ]]; then
        cat "$LLM_OUTPUT"
    else
        dump_result "{agent_name}:${LLM_AGENT_FUNC}"
    fi
}

dump_result() {
    if [[ "$LLM_OUTPUT" == "/dev/stdout" ]] || [[ -z "$LLM_DUMP_RESULTS" ]] ||  [[ ! -t 1 ]]; then
        return;
    fi

    if grep -q -w -E "$LLM_DUMP_RESULTS" <<<"$1"; then
        cat <<EOF
$(echo -e "\e[2m")----------------------
$(cat "$LLM_OUTPUT")
----------------------$(echo -e "\e[0m")
EOF
    fi
}

die() {
    echo "$*" >&2
    exit 1
}

main "$@"
