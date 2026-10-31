#!/usr/bin/env bash
# yq replication of the wiki's session-block / schema rules for sisyphus-next
# (Graph-Agents "Validation" + "Sessions"); mirrors the facade's own check recipe.
# Usage: check_session_block.sh [graph.yaml] [engineer config.yaml]
set -euo pipefail
G="${1:-$(dirname "$0")/../graph.yaml}"
ENG="${2:-$(dirname "$0")/../../engineer/config.yaml}"
fail=0
say() { printf '%s %s\n' "$1" "$2"; [[ "$1" == FAIL ]] && fail=1 || true; }

[[ "$(yq '.version' "$G")" == "1.0" ]] && say ok "version is 1.0" || say FAIL "version must be 1.0"
[[ "$(yq '.session.chat.agent' "$G")" == "engineer" ]] && say ok "session.chat.agent = engineer" || say FAIL "chat.agent"
[[ "$(yq '.session.chat | has("instructions") or has("model")' "$G")" == "false" ]] && say ok "chat.agent exclusive with instructions/model" || say FAIL "chat.agent exclusivity"
[[ "$(yq '.session.chat.memory // false' "$G")" == "false" && "$(yq '.session.chat.tools // [] | length' "$G")" == "0" ]] && say ok "chat.memory false / chat.tools empty beside agent:" || say FAIL "memory/tools beside agent"
[[ "$(yq '.session.chat.agent' "$G")" != "$(yq '.name' "$G")" ]] && say ok "chat.agent is not the graph itself" || say FAIL "self chat agent"
grep -q '__INPUT__' "$G" && say FAIL "__INPUT__ placeholder present" || say ok "no __INPUT__ placeholder"
for k in $(yq '.session.chat.variables | keys | .[]' "$G"); do
  yq -e ".variables[] | select(.name == \"$k\")" "$ENG" >/dev/null && say ok "engineer declares chat variable $k" || say FAIL "engineer lacks variable $k"
done
# every expose key declared (initial_state, a variable, state_updates key, output_schema property, or collect_into)
declared=$(yq '[(.initial_state | keys | .[]), (.variables[].name), (.nodes[].state_updates // {} | keys | .[]), (.nodes[].output_schema.properties // {} | keys | .[]), (.nodes[] | select(.type == "map") | .collect_into)] | unique | .[]' "$G")
for k in $(yq '.session.expose[]' "$G"); do
  grep -qx "$k" <<<"$declared" && say ok "expose key declared: $k" || say FAIL "expose key undeclared: $k"
done
dups=$(yq '.session.expose | length' "$G"); uniq_=$(yq '.session.expose | unique | length' "$G")
[[ "$dups" == "$uniq_" ]] && say ok "expose keys listed once" || say FAIL "duplicate expose keys"
# variable shadowing: no initial_state key may share a variable's name
for v in $(yq '.variables[].name' "$G"); do
  [[ "$(yq ".initial_state | has(\"$v\")" "$G")" == "false" ]] && say ok "variable not shadowed: $v" || say FAIL "variable shadowed by initial_state: $v"
done
# variables mirror the prose sisyphus: names + defaults exactly
CORE="$(dirname "$0")/../../sisyphus/config.yaml"
if [[ -f "$CORE" ]]; then
  a=$(yq -o=json '[.variables[] | {"name": .name, "default": .default}]' "$G"); b=$(yq -o=json '[.variables[] | {"name": .name, "default": .default}]' "$CORE")
  [[ "$a" == "$b" ]] && say ok "variables mirror $(basename "$(dirname "$CORE")") (names + defaults)" || say FAIL "variable mirror drift vs $CORE"
else
  say FAIL "no prose sisyphus config.yaml found to mirror against"
fi
# every approval option has a route; every approval has on_other; every input has validation grammar len(input) <op> N
for n in $(yq '.nodes[] | select(.type == "approval") | .id' "$G"); do
  miss=$(yq ".nodes.$n | .options - (.routes | keys) | length" "$G")
  [[ "$miss" == "0" && "$(yq ".nodes.$n.on_other" "$G")" != "null" ]] && say ok "approval $n: routes cover options + on_other" || say FAIL "approval $n routes/on_other"
done
for n in $(yq '.nodes[] | select(.type == "input") | .id' "$G"); do
  yq ".nodes.$n.validation" "$G" | grep -Eq '^len\(input\) (>|>=|<|<=|==) [0-9]+$' && say ok "input $n: validation grammar" || say FAIL "input $n validation grammar"
done
# llm tools whitelist subset of global_tools (.sh stripped); node skills subset of graph skills; map as != output_key
gt=$(yq '.global_tools[]' "$G" | sed 's/\.sh$//')
for t in $(yq '[.nodes[] | select(.type == "llm") | .tools // [] | .[]] | unique | .[]' "$G"); do
  grep -qx "$t" <<<"$gt" && say ok "llm tool declared globally: $t" || say FAIL "llm tool not in global_tools: $t"
done
gs=$(yq '.enabled_skills[]' "$G")
for s in $(yq '[.nodes[] | .enabled_skills // [] | .[]] | unique | .[]' "$G"); do
  grep -qx "$s" <<<"$gs" && say ok "node skill in graph universe: $s" || say FAIL "node skill outside universe: $s"
done
for n in $(yq '.nodes[] | select(.type == "map") | .id' "$G"); do
  [[ "$(yq ".nodes.$n.as" "$G")" != "$(yq ".nodes.$n.output_key" "$G")" ]] && say ok "map $n: as != output_key" || say FAIL "map $n as == output_key"
done
# every node id matches its key; every script path exists
for n in $(yq '.nodes | keys | .[]' "$G"); do
  [[ "$(yq ".nodes.$n.id" "$G")" == "$n" ]] || say FAIL "node id mismatch: $n"
done
for s in $(yq '.nodes[] | select(.type == "script") | .script' "$G"); do
  [[ -f "$(dirname "$G")/$s" ]] || say FAIL "missing script $s"
done
say ok "node ids / script paths consistent"
exit $fail
