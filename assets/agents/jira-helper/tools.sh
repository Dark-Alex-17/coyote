#!/usr/bin/env bash
# shellcheck disable=SC2154
# shellcheck disable=SC2046
set -e

# @meta require-tools jira
# @env LLM_OUTPUT=/dev/stdout The output path
# @env LLM_AGENT_VAR_CONFIG! The configuration to use for the Jira CLI; e.g. work
# @env LLM_AGENT_VAR_PROJECT! The Jira project to operate on; e.g. PAN

# @cmd Fetch my Jira username
get_jira_username() {
  declare config_file="$HOME/.config/.jira/${LLM_AGENT_VAR_CONFIG}.yml"

  jira me -c "$config_file" >> "$LLM_OUTPUT"
}

# @cmd Query for jira issues using a Jira Query Language (JQL) query
# @option --jql-query! The Jira Query Language query to execute
# @option --project! $LLM_AGENT_VAR_PROJECT <PROJECT> Jira project to operate on; e.g. PAN
query_jira_issues() {
  declare config_file="$HOME"/.config/.jira/"${LLM_AGENT_VAR_CONFIG}".yml

  jira issue ls \
    --project "$argc_project" \
    -q "$argc_jql_query" \
    --plain \
    -c "$config_file" >> "$LLM_OUTPUT"
}

# @cmd Assign a Jira issue to the specified user
# @option --issue-key! The Jira issue key, e.g. ISSUE-1
# @option --assignee! The email or display name of the user to assign the issue to
# @option --project! $LLM_AGENT_VAR_PROJECT <PROJECT> Jira project to operate on; e.g. PAN
assign_jira_issue() {
  declare config_file="$HOME"/.config/.jira/"${LLM_AGENT_VAR_CONFIG}".yml

  jira issue assign \
    --project "$argc_project" \
    "$argc_issue_key" "$argc_assignee" \
    -c "$config_file" >> "$LLM_OUTPUT"
}

# @cmd View a Jira issue
# @option --issue-key! The Jira issue key, e.g. ISSUE-1
# @option --project! $LLM_AGENT_VAR_PROJECT <PROJECT> Jira project to operate on; e.g. PAN
view_issue() {
  declare config_file="$HOME"/.config/.jira/"${LLM_AGENT_VAR_CONFIG}".yml

  jira issue view \
    "$argc_issue_key" \
    --project "$argc_project" \
    --comments 20 \
    --plain \
    -c "$config_file" >> "$LLM_OUTPUT"
}

# @cmd Transition a Jira issue to a different state
# @option --issue-key! The Jira issue key, e.g. ISSUE-1
# @option --state![`_issue_state_choice`] The Jira state of the issue
# @option --comment Add a comment to the issue
# @option --resolution Set resolution
# @option --project! $LLM_AGENT_VAR_PROJECT <PROJECT> Jira project to operate on; e.g. PAN
transition_issue() {
  declare config_file="$HOME"/.config/.jira/"${LLM_AGENT_VAR_CONFIG}".yml
  declare -a flags=()

  if [[ -n $argc_comment ]]; then
    flags+=("--comment '${argc_comment}'")
  fi

  if [[ -n $argc_resolution ]]; then
    flags+=("--resolution ${argc_resolution}")
  fi

  jira issue move \
    --project "$argc_project" \
    "$argc_issue_key" "$argc_state" "$(echo "${flags[*]}" | xargs)" \
    -c "$config_file" >> "$LLM_OUTPUT"
}

# @cmd Create a new Jira issue
# @option --type![`_issue_type_choice`]
# @option --summary! Issue summary or title
# @option --description! Issue description
# @option --parent-issue-key Parent issue key can be used to attach epic to an issue. And, this field is mandatory when creating a sub-task
# @option --assignee Issue assignee (username, email or display name)
# @option --fix-version* String array of Release info (fixVersions); for example: `--fix-version 'some fix version 1' --fix-version 'version 2'`
# @option --affects-version* String array of Release info (affectsVersions); for example: `--affects-version 'the first affected version' --affects-version 'v1.2.3'`
# @option --label* String array of issue labels; for example: `--label backend --label custom`
# @option --component* String array of issue components; for example: `--component backend --component core`
# @option --original-estimate The original estimate of the issue
# @option --priority[=Medium|Highest|High|Low|Lowest] The priority of the issue
# @option --project! $LLM_AGENT_VAR_PROJECT <PROJECT> Jira project to operate on; e.g. PAN
create_issue() {
  declare config_file="$HOME"/.config/.jira/"${LLM_AGENT_VAR_CONFIG}".yml
  declare -a flags=()

  if [[ -n $argc_assignee ]]; then
    flags+=("--assignee $argc_assignee")
  fi

  if [[ -n $argc_original_estimate ]]; then
    flags+=("--original-estimate $argc_original_estimate")
  fi

  if [[ -n $argc_priority ]]; then
    flags+=("--priority $argc_priority")
  fi

  if [[ -n $argc_fix_version ]]; then
    for version in "${argc_fix_version[@]}"; do
      flags+=("--fix-version '$version'")
    done
  fi

  if [[ -n $argc_affects_version ]]; then
    for version in "${argc_affects_version[@]}"; do
      flags+=("--affects-version '$version'")
    done
  fi

  if [[ -n $argc_components ]]; then
    for component in "${argc_components[@]}"; do
      flags+=("--affects-version '$component'")
    done
  fi

  jira issue create \
    --project "$argc_project" \
    --type "$argc_type" \
    --summary "$argc_summary" \
    --body "$argc_description" \
    --parent "$argc_parent_issue_key" \
    -c "$config_file" \
    --no-input $(echo "${flags[*]}" | xargs) >> "$LLM_OUTPUT"
}

# @cmd Link two issues together
# @option --inward-issue-key! Issue key of the source issue, eg: ISSUE-1
# @option --outward-issue-key! Issue key of the target issue, eg: ISSUE-2
# @option --issue-link-type! Relationship between two issues, eg: Duplicates, Blocks etc.
# @option --project! $LLM_AGENT_VAR_PROJECT <PROJECT> Jira project to operate on; e.g. PAN
link_issues() {
  declare config_file="$HOME"/.config/.jira/"${LLM_AGENT_VAR_CONFIG}".yml

  jira issue link \
    --project "$argc_project" \
    "${argc_inward_issue_key}" "${argc_outward_issue_key}" "${argc_issue_link_type}" \
    -c "$config_file" >> "$LLM_OUTPUT"
}

# @cmd Unlink or disconnect two issues from each other, if already connected.
# @option --inward-issue-key! Issue key of the source issue, eg: ISSUE-1
# @option --outward-issue-key! Issue key of the target issue, eg: ISSUE-2.
# @option --project! $LLM_AGENT_VAR_PROJECT <PROJECT> Jira project to operate on; e.g. PAN
unlink_issues() {
  declare config_file="$HOME"/.config/.jira/"${LLM_AGENT_VAR_CONFIG}".yml

  jira issue unlink \
    --project "$argc_project" \
    "${argc_inward_issue_key}" "${argc_outward_issue_key}" \
    -c "$config_file" >> "$LLM_OUTPUT"
}

# @cmd Add a comment to an issue
# @option --issue-key! Issue key of the source issue, eg: ISSUE-1
# @option --comment-body! Body of the comment you want to add
# @option --project! $LLM_AGENT_VAR_PROJECT <PROJECT> Jira project to operate on; e.g. PAN
add_comment_to_issue() {
  declare config_file="$HOME"/.config/.jira/"${LLM_AGENT_VAR_CONFIG}".yml

  jira issue comment add \
    --project "$argc_project" \
    "${argc_issue_key}" "${argc_comment_body}" \
    --no-input \
    -c "$config_file" >> "$LLM_OUTPUT"
}

# @cmd Edit an existing Jira issue
# @option --issue-key! The Jira issue key, e.g. ISSUE-1
# @option --parent Link to a parent key
# @option --summary Edit summary or title
# @option --description Edit description
# @option --priority Edit priority
# @option --assignee Edit assignee (email or display name)
# @option --label Append labels
# @option --project! $LLM_AGENT_VAR_PROJECT <PROJECT> Jira project to operate on; e.g. PAN
edit_issue() {
  declare config_file="$HOME"/.config/.jira/"${LLM_AGENT_VAR_CONFIG}".yml
  declare -a flags=()

  if [[ -n $argc_parent ]]; then
    flags+=("--parent $argc_parent")
  fi

  if [[ -n $argc_summary ]]; then
    flags+=("--summary $argc_summary")
  fi

  if [[ -n $argc_description ]]; then
    flags+=("--body $argc_description")
  fi

  if [[ -n $argc_priority ]]; then
    flags+=("--priority $argc_priority")
  fi

  if [[ -n $argc_assignee ]]; then
    flags+=("--assignee $argc_assignee")
  fi

  if [[ -n $argc_label ]]; then
    flags+=("--label $argc_label")
  fi

  jira issue edit \
    --project "$argc_project" \
    "$argc_issue_key" $(echo "${flags[*]}" | xargs) \
    --no-input \
    -c "$config_file" >> "$LLM_OUTPUT"
}

_issue_type_choice() {
  if [[ $LLM_AGENT_VAR_CONFIG == "work" ]]; then
    echo "Story"
    echo "Task"
    echo "Bug"
    echo "Technical Debt"
    echo "Sub-task"
  elif [[ $LLM_AGENT_VAR_CONFIG == "sideproject" ]]; then
    echo "Task"
    echo "Story"
    echo "Bug"
    echo "Epic"
  fi
}

_issue_state_choice() {
  if [[ $LLM_AGENT_VAR_CONFIG == "work" ]]; then
    echo "Ready for Dev"
    echo "CODE REVIEW"
    echo "IN PROGRESS"
    echo "Backlog"
    echo "Done"
    echo "TESTING"
  elif [[ $LLM_AGENT_VAR_CONFIG == "sideproject" ]]; then
    echo "IN CLARIFICATION"
    echo "NEED TO CLARIFY"
    echo "READY TO WORK"
    echo "RELEASE BACKLOG"
    echo "REOPEN"
    echo "CODE REVIEW"
    echo "IN PROGRESS"
    echo "IN TESTING"
    echo "TO TEST"
    echo "DONE"
  fi
}
