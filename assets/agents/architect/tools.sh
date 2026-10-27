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

# @cmd Collect tagged ledger lines from task log.md files - the grep-complete rollup for Phase F reports, hygiene briefs, and PR bodies (rejected-finding: / design-decision / Follow-ups: / debris-exempt / "(deferred by quality bar)"). Prints one "<task> (log.md:NN): <line>" per hit grouped by task, with the block under a tag line (its bullets, up to a blank line, the next ## heading, or the next tag line) indented beneath it, or "(none)" - a missed line is a tool bug, not an attention slip.
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
  # <dir>/<task>/log.md:NN:line  ->  <task> (log.md:NN): line, then the block
  # under line NN indented (bullets and their indented wraps, verbatim), so a
  # bare "Follow-ups:" line brings its bullets along. A column-0 line that is
  # not a bullet is wrapped prose, not part of the block.
  local line path rest rel task lineno
  while IFS= read -r line; do
    path="${line%%:*}"
    rest="${line#*:}"
    rel="${path#"${dir}"/}"
    task="${rel%/log.md}"
    lineno="${rest%%:*}"
    echo "${task} (log.md:${lineno}): ${rest#*:}" >> "$LLM_OUTPUT"
    awk -v start="${lineno}" '
      NR <= start { next }
      /^[[:space:]]*$/ || /^## / { exit }
      !/^[[:space:]]/ && !/^[-*][[:space:]]/ { exit }
      { head = $0; sub(/^[[:space:]]*([-*][[:space:]]+)?/, "", head) }
      head ~ /^(rejected-finding:|design-decision|Follow-ups:|HYGIENE:)/ { exit }
      { print "  " $0 }
    ' "${path}" >> "$LLM_OUTPUT"
  done <<< "${hits}"
}

# @cmd Assemble the single draft PR's body from the plan + tasks tree: your summary file verbatim, the Quality bar line (plan frontmatter rigor, below production only), the task checklist (title + log.md completed-entry SHAs), and the grep-complete ledger sections (Review decisions / Hygiene pass / Design decisions, optional Follow-ups). Prints the body (and NOTE lines for anything it could not derive); review it, then pass it to gh pr create --body-file.
# @option --plan! Path to PLAN-<slug>.md (rigor read from frontmatter; its name with the `.md` stripped selects the tasks whose `plan:` field names it, with or without `.md`)
# @option --tasks-dir! Tree searched recursively for TASK-NNN-*/index.md + log.md (the plans tasks dir)
# @option --tasks Comma-separated task ids (TASK-a,TASK-b): keep only those in the checklist and ledger rollups - a PR group's members under pr_strategy=multi
# @option --summary-file! File holding the Problem/Approach summary you wrote - pasted verbatim at the top
# @option --hygiene-file File holding the kept HYGIENE items verbatim (default: derived from `HYGIENE:` ledger lines, else "clean")
# @option --quality-note Text appended to the Quality bar line (e.g. "deferred hardening tracked in TASK-NNN")
# @option --out Also write the body to this file (hand it to `gh pr create --body-file`)
# @flag --followups Append a `## Follow-up / manual actions` section from `Follow-ups:` ledger lines
assemble_pr_body() {
  local -a args=(--plan "${argc_plan}" --tasks-dir "${argc_tasks_dir}" --summary-file "${argc_summary_file}")
  [[ -n "${argc_tasks:-}" ]] && args+=(--tasks "${argc_tasks}")
  [[ -n "${argc_hygiene_file:-}" ]] && args+=(--hygiene-file "${argc_hygiene_file}")
  [[ -n "${argc_quality_note:-}" ]] && args+=(--quality-note "${argc_quality_note}")
  [[ -n "${argc_out:-}" ]] && args+=(--out "${argc_out}")
  [[ -n "${argc_followups:-}" ]] && args+=(--followups)
  # stderr carries the NOTE lines - the model must see them next to the body.
  python3 "$LLM_AGENT_ROOT_DIR/assemble_pr_body.py" "${args[@]}" >> "$LLM_OUTPUT" 2>&1
}
