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

# @cmd Collect tagged ledger lines from the plan's completed tasks' log.md files - the grep-complete rollup for Phase F reports, hygiene briefs, and PR bodies (rejected-finding: / design-decision / Follow-ups: / debris-exempt / "(deferred by quality bar)"). Only task dirs whose index.md names the plan (`plan:`, with or without `.md`) and is `status: complete` are read, so a tasks dir shared by several plans never leaks another run's lines; a task whose index.md has no `plan:` or no `status:` field is kept and named in a NOTE. Prints one "<task> (log.md:NN): <line>" per hit grouped by task, with the block under a tag line (its bullets, up to a blank line, the next ## heading, or the next tag line) indented beneath it, or "(none)" - a missed line is a tool bug, not an attention slip.
# @option --tag! Literal tag text to collect (fixed string, not a regex)
# @option --plan! Path to PLAN-<slug>.md; its name with the `.md` stripped selects the tasks whose `plan:` field names it
# @option --dir! The plans tasks dir, searched recursively for task dirs; each listed task's own log.md is read
# @option --tasks Comma-separated task ids (TASK-a,TASK-b): read only those tasks - a PR group's members under pr_strategy=multi
collect_ledger_lines() {
  local dir="${argc_dir}"
  local tag="${argc_tag}"
  if [[ ! -d "${dir}" ]]; then
    echo "collect_ledger_lines: directory not found: ${dir}" >> "$LLM_OUTPUT"
    return 1
  fi
  # The plan's completed task dirs (plan_tasks.py: same identity rule as
  # assemble_pr_body), as NUL-terminated <dir>/<rel> and escaped-label pairs:
  # a dir name may hold a newline, and a newline split would hand its tail
  # (`..`, say) to grep as a path of its own. The listing lands in a file
  # because `$(...)` drops NUL bytes. Its NOTE lines go to fd 3, opened on
  # LLM_OUTPUT outside the redirection, so with LLM_OUTPUT=/dev/stdout they
  # reach the output, never the listing.
  local -a scope=(--labels --plan "${argc_plan}" --tasks-dir "${dir}")
  [[ -n "${argc_tasks:-}" ]] && scope+=(--tasks "${argc_tasks}")
  local listing listed=0
  if ! listing=$(mktemp "${TMPDIR:-/tmp}/collect_ledger_lines.XXXXXX"); then
    echo "collect_ledger_lines: could not create a temp file under ${TMPDIR:-/tmp}" >> "$LLM_OUTPUT"
    return 1
  fi
  # Expanded now: the local is gone by the time the shell exits.
  trap "rm -f -- $(printf '%q' "${listing}")" EXIT
  { python3 -B "$LLM_AGENT_ROOT_DIR/plan_tasks.py" "${scope[@]}" > "${listing}" 2>&3 || listed=$?; } 3>> "$LLM_OUTPUT"
  if [[ "${listed}" -ne 0 ]]; then
    echo "collect_ledger_lines: could not list the plan's tasks under ${dir}" >> "$LLM_OUTPUT"
    return 1
  fi
  # Only <task dir>/log.md is read, never a log.md deeper down (a nested dir
  # is a task of its own, listed or not on its own terms); [[ -f ]] follows a
  # symlinked log.md the way assemble_pr_body does. -F: the tag is a literal,
  # never a pattern; -I skips binaries; grep exit 1 (no matches) is the clean
  # result, not an error.
  # Each NN:line prints as <label> (log.md:NN): line, then the block under
  # line NN indented (bullets and their indented wraps, verbatim), so a bare
  # "Follow-ups:" line brings its bullets along. A column-0 line that is not a
  # bullet is wrapped prose, not part of the block. awk reads the log on stdin
  # because a file operand shaped like `a=b/...` is taken as an assignment.
  local d label hit lineno found=0
  while IFS= read -r -d '' d && IFS= read -r -d '' label; do
    [[ -f "${d}/log.md" ]] || continue
    while IFS= read -r hit; do
      found=1
      lineno="${hit%%:*}"
      echo "${label} (log.md:${lineno}): ${hit#*:}" >> "$LLM_OUTPUT"
      awk -v start="${lineno}" '
        NR <= start { next }
        /^[[:space:]]*$/ || /^## / { exit }
        !/^[[:space:]]/ && !/^[-*][[:space:]]/ { exit }
        { head = $0; sub(/^[[:space:]]*([-*][[:space:]]+)?/, "", head) }
        head ~ /^(rejected-finding:|design-decision|Follow-ups:|HYGIENE:)/ { exit }
        { print "  " $0 }
      ' < "${d}/log.md" >> "$LLM_OUTPUT"
    done < <(grep -nIF -- "${tag}" "${d}/log.md" 2>/dev/null || true)
  done < "${listing}"
  if [[ "${found}" -eq 0 ]]; then
    echo "(none)" >> "$LLM_OUTPUT"
  fi
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
  python3 -B "$LLM_AGENT_ROOT_DIR/assemble_pr_body.py" "${args[@]}" >> "$LLM_OUTPUT" 2>&1
}
