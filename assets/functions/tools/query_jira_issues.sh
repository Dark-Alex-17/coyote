#!/usr/bin/env bash
set -e

# @meta require-tools jira
# @describe Query for jira issues using a Jira Query Language (JQL) query
# @option --jql-query! The Jira Query Language query to execute
# @env LLM_OUTPUT=/dev/stdout The output path

main() {
  jira issue ls -q "$argc_jql_query" --plain >> "$LLM_OUTPUT"
}