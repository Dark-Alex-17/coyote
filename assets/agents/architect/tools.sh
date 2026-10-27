#!/usr/bin/env bash
set -eo pipefail
# shellcheck disable=SC1090
source "$LLM_ROOT_DIR/agents/.shared/utils.sh"

# @env LLM_OUTPUT=/dev/stdout
# @describe Architect agent internal tools

# @cmd Internal dynamic-instructions hook - renders instructions.md against the agent variables. Not for model use.
_instructions() {
  render_instructions "$LLM_AGENT_ROOT_DIR/instructions.md" >> "$LLM_OUTPUT"
}

# @cmd Collect tagged ledger lines from task log.md files - the grep-complete rollup for Phase F reports, hygiene briefs, and PR bodies (rejected-finding: / design-decision / debris-exempt / "(deferred by quality bar)"). Prints one "<task>: <line>" per hit grouped by task, or "(none)" - a missed line is a tool bug, not an attention slip.
# @option --tag! Literal tag text to collect (fixed string, not a regex)
# @option --dir! Directory searched recursively for log.md files (the plans tasks dir)
collect_ledger_lines() {
  local dir="${argc_dir}"
  local tag="${argc_tag}"
  if [[ ! -d "${dir}" ]]; then
    echo "collect_ledger_lines: directory not found: ${dir}" >> "$LLM_OUTPUT"
    return 1
  fi
  local hits
  # -F: the tag is a literal, never a pattern; -I skips binaries; grep exit 1
  # (no matches) is the clean result, not an error.
  hits=$(grep -rInF --include=log.md -- "${tag}" "${dir}" 2>/dev/null) || true
  if [[ -z "${hits}" ]]; then
    echo "(none)" >> "$LLM_OUTPUT"
    return 0
  fi
  # <dir>/<task>/log.md:NN:line  ->  <task> log.md:NN: line
  local line path rest rel task
  while IFS= read -r line; do
    path="${line%%:*}"
    rest="${line#*:}"
    rel="${path#"${dir}"/}"
    task="${rel%/log.md}"
    echo "${task} (log.md:${rest%%:*}): ${rest#*:}" >> "$LLM_OUTPUT"
  done <<< "${hits}"
}

# @cmd Assemble the single draft PR's body from the plan + tasks tree: your summary file verbatim, the Quality bar line (plan frontmatter rigor, below production only), the task checklist (title + log.md completed-entry SHAs), and the grep-complete ledger sections (Review decisions / Hygiene pass / Design decisions, optional Follow-ups). Prints the body (and NOTE lines for anything it could not derive); review it, then pass it to gh pr create --body-file.
# @option --plan! Path to PLAN-<slug>.md (rigor read from frontmatter; its basename selects the tasks that carry a matching `plan:` field)
# @option --tasks-dir! Tree searched recursively for TASK-NNN-*/index.md + log.md (the plans tasks dir)
# @option --summary-file! File holding the Problem/Approach summary you wrote - pasted verbatim at the top
# @option --hygiene-file File holding the kept HYGIENE items verbatim (default: derived from `HYGIENE:` ledger lines, else "clean")
# @option --quality-note Text appended to the Quality bar line (e.g. "deferred hardening tracked in TASK-NNN")
# @option --out Also write the body to this file (hand it to `gh pr create --body-file`)
# @flag --followups Append a `## Follow-up / manual actions` section from `Follow-ups:` ledger lines
assemble_pr_body() {
  local -a args=(--plan "${argc_plan}" --tasks-dir "${argc_tasks_dir}" --summary-file "${argc_summary_file}")
  [[ -n "${argc_hygiene_file:-}" ]] && args+=(--hygiene-file "${argc_hygiene_file}")
  [[ -n "${argc_quality_note:-}" ]] && args+=(--quality-note "${argc_quality_note}")
  [[ -n "${argc_out:-}" ]] && args+=(--out "${argc_out}")
  [[ -n "${argc_followups:-}" ]] && args+=(--followups)
  # stderr carries the NOTE lines - the model must see them next to the body.
  python3 "$LLM_AGENT_ROOT_DIR/assemble_pr_body.py" "${args[@]}" >> "$LLM_OUTPUT" 2>&1
}
