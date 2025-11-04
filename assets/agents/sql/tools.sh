#!/usr/bin/env bash

set -e

# @meta require-tools usql
# @env LLM_OUTPUT=/dev/stdout The output path
# @env LLM_AGENT_VAR_DSN! The database connection url. e.g. pgsql://user:pass@host:port 

# shellcheck disable=SC1090
source "$LLM_PROMPT_UTILS_FILE"

# @cmd Execute a SELECT query
# @option --query!                  SELECT SQL query to execute
read_query() {
    # shellcheck disable=SC2154
    if ! grep -qi '^select' <<<"$argc_query"; then
        error "only SELECT queries are allowed" >&2
        exit 1
    fi

    _run_sql "$argc_query"
}

# @cmd Execute an SQL query
# @option --query!                  SQL query to execute
write_query() {
    guard_operation "Execute SQL?"
    _run_sql "$argc_query"
}

# @cmd List all tables
list_tables() {
    _run_sql "\dt+"
}

# @cmd Get the schema information for a specific table
# @option --table-name!             Name of the table to describe
describe_table() {
    # shellcheck disable=SC2154
    _run_sql "\d $argc_table_name"
}

_run_sql() {
    usql "$LLM_AGENT_VAR_DSN" -c "$1" >> "$LLM_OUTPUT"
}
