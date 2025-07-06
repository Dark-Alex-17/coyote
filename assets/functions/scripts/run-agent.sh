#!/usr/bin/env bash

# Usage: ./{agent_name}.sh <agent-func> <agent-data>

set -e

main() {
    root_dir="{config_dir}"
    parse_argv "$@"
    setup_env
    tools_path="$root_dir/agents/{agent_name}/tools.sh"
    run
}

parse_argv() {
		agent_func="$1"
		agent_data="$2"
    if [[ -z "$agent_data" ]] || [[ -z "$agent_func" ]]; then
        die "usage: ./{agent_name}.sh <agent-func> <agent-data>"
    fi
}

setup_env() {
    load_env "$root_dir/.env"
    export LLM_ROOT_DIR="$root_dir"
    export LLM_AGENT_NAME="{agent_name}"
    export LLM_AGENT_FUNC="$agent_func"
    export LLM_AGENT_ROOT_DIR="$LLM_ROOT_DIR/agents/{agent_name}"
    export LLM_AGENT_CACHE_DIR="$LLM_ROOT_DIR/cache/{agent_name}"
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

    if [[ "$OS" == "Windows_NT" ]]; then
        set -o igncr
        tools_path="$(cygpath -w "$tools_path")"
        tool_data="$(echo "$tool_data" | sed 's/\\/\\\\/g')"
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
        .value | .[] | "--\($key) \(. | escape_shell_word)"
    elif .value | type == "boolean" then
        if .value then "--\($key)" else "" end
    else
        "--\($key) \(.value | escape_shell_word)"
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

