#!/usr/bin/env python3
"""Static audit of the sisyphus-next graph.

Checks: every declared edge (next / fallback / branch / approval routes /
on_other) and every script-emitted `_next` resolves to a real node; map branch
subgraphs stay disjoint from the main flow; every cycle (static + dynamic)
passes through a node that owns a named bound (section 5); the engine backstop
is set. Script `_next` targets are harvested per gate function from gates.py
by AST (string constants assigned to `u["_next"]` and the shared
`raise_escalation` / `_terminal_prompt` helpers), so the dynamic edges are
attributed to the node that emits them, not smeared over every script node.

Usage: audit_graph.py [agent dir]   (exit 1 on any problem)
"""

import ast
import json
import os
import re
import subprocess
import sys
from collections import defaultdict

AGENT_DIR = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(__file__), ".."))

# Node -> bound id(s) (the named loop bounds). A cycle is legal
# only if it passes through at least one of these nodes.
BOUNDED = {
    "route_intent": "B1 clarify_rounds<=3",
    "record_subtasks": "B2 subtasks<=3",
    "next_subtask": "B3 list-bounded subtask index",
    "oracle_gate": "B4 once per task",
    "route_coder_result": "B5 fix_strikes<=3 / B7 rejected_rounds<=3",
    "apply_answer": "B6 strikeout_rounds<=1 / B9 design_ask_rounds<=1 / user-driven choices",
    "bookkeeping_route": "B8 debt_rounds<=1 / B9 design ask once",
    "triage_route": "B10 review_rounds<=3 + lane streak<=1",
    "route_gauntlet": "B11 debris_reruns<=1",
    "observability_route": "B12 obs_rounds<=1",
    "ask_owner_route": "B13 owner_polls<=2",
    "parse_answer": "B14 reask_rounds<=3",
    "ci_poll": "B15 wall clock + poll fails<=3",
    "ci_apply": "B15 flake reruns<=2 per check + fix strikes<=3",
    "merge_oracle": "oracle_retries<=1 (via apply_answer)",
    "scope_check": "scope_confirmed once / breach -> escalation",
}


def load_graph():
    p = subprocess.run(["yq", "-o=json", ".", os.path.join(AGENT_DIR, "graph.yaml")], capture_output=True, text=True)
    if p.returncode != 0:
        sys.exit(f"yq failed: {p.stderr}")
    return json.loads(p.stdout)


def gate_for_script(path):
    src = open(path, encoding="utf-8").read()
    m = re.search(r'run\("([a-z_]+)"\)', src)
    return m.group(1) if m else None


def harvest_gate_targets(gates_src):
    """gate function name -> set of `_next` literals it can emit (incl. helpers)."""
    tree = ast.parse(gates_src)
    funcs = {n.name: n for n in tree.body if isinstance(n, ast.FunctionDef)}

    def literals(fn):
        out = set()
        for node in ast.walk(fn):
            if isinstance(node, ast.Assign):
                for t in node.targets:
                    if (isinstance(t, ast.Subscript) and isinstance(t.slice, ast.Constant) and t.slice.value == "_next"
                            and isinstance(node.value, ast.Constant)):
                        out.add(node.value.value)
                    if (isinstance(t, ast.Subscript) and isinstance(t.slice, ast.Constant) and t.slice.value == "_next"
                            and isinstance(node.value, ast.IfExp)):
                        for part in (node.value.body, node.value.orelse):
                            if isinstance(part, ast.Constant):
                                out.add(part.value)
            if isinstance(node, ast.Dict):
                for k, v in zip(node.keys, node.values):
                    if isinstance(k, ast.Constant) and k.value == "_next" and isinstance(v, ast.Constant):
                        out.add(v.value)
            if isinstance(node, ast.Call):
                name = getattr(node.func, "id", None) or getattr(node.func, "attr", None)
                if name == "raise_escalation":
                    out.add("escalate_gate")
                if name == "_terminal_prompt":
                    out.update({"ask_user", "ask_review_incomplete"})
                if name in funcs and name.startswith("_") and name not in ("_terminal_prompt",):
                    out.update(literals(funcs[name]))
        return out

    return {name: literals(fn) for name, fn in funcs.items()}


def _in_branch(nodes, nid):
    """True when nid is reachable from some map's branch entry via next/fallback (chain ends by omission)."""
    for n in nodes.values():
        if n.get("type") != "map":
            continue
        seen, st = set(), [n["branch"]]
        while st:
            u = st.pop()
            if u in seen or u not in nodes:
                continue
            seen.add(u)
            nn = nodes[u]
            st += [nn.get("fallback")] if nn.get("fallback") else []
            nx = nn.get("next")
            st += nx if isinstance(nx, list) else [nx] if nx else []
        if nid in seen:
            return True
    return False


def main():
    g = load_graph()
    nodes = g["nodes"]
    problems, edges = [], []
    gates_src = open(os.path.join(AGENT_DIR, "scripts", "gates.py"), encoding="utf-8").read()
    gate_targets = harvest_gate_targets(gates_src)
    dyn = defaultdict(set)

    for nid, n in nodes.items():
        if n.get("id") != nid:
            problems.append(f"{nid}: id field mismatch ({n.get('id')!r})")
        nxt = n.get("next")
        for t in (nxt if isinstance(nxt, list) else [nxt] if nxt else []):
            edges.append((nid, t, "next"))
        for k in ("fallback", "branch", "on_other"):
            if n.get(k):
                edges.append((nid, n[k], k))
        for t in (n.get("routes") or {}).values():
            edges.append((nid, t, "route"))
        if n.get("type") == "script":
            sp = os.path.join(AGENT_DIR, n["script"])
            if not os.path.exists(sp):
                problems.append(f"{nid}: missing script {n['script']}")
                continue
            gate = gate_for_script(sp)
            if gate is None and nid != "debt_scan":
                problems.append(f"{nid}: entry script names no gate")
            for t in gate_targets.get(gate, set()):
                dyn[nid].add(t)
                edges.append((nid, t, "_next"))
            if not n.get("next") and not dyn[nid] and not n.get("fallback") and not _in_branch(nodes, nid):
                problems.append(f"{nid}: script with neither next, fallback nor _next")
    for s, d, kind in edges:
        if d not in nodes:
            problems.append(f"{s} -> {d!r} ({kind}) nonexistent")

    # map branch subgraphs: disjoint from the main flow; chains end by omission
    main_reach, stack = set(), [g["start"]]
    branch_entries = {n["branch"] for n in nodes.values() if n.get("type") == "map"}
    adj_static = defaultdict(set)
    for s, d, kind in edges:
        if kind != "branch" and d in nodes:
            adj_static[s].add(d)
    while stack:
        u = stack.pop()
        if u in main_reach:
            continue
        main_reach.add(u)
        for v in adj_static[u]:
            if v not in branch_entries:
                stack.append(v)
    for entry in branch_entries:
        sub, st = set(), [entry]
        while st:
            u = st.pop()
            if u in sub:
                continue
            sub.add(u)
            st.extend(adj_static[u])
        for x in sub:
            if nodes[x]["type"] not in ("llm", "agent", "rag", "script"):
                problems.append(f"map branch {entry}: node {x} has type {nodes[x]['type']}")
            if x in main_reach:
                problems.append(f"map branch {entry}: node {x} also reachable from the main flow")

    # cycles over static + dynamic edges
    adj = defaultdict(set)
    for s, d, _ in edges:
        if d in nodes:
            adj[s].add(d)
    cycles, color, path = [], {}, []

    def dfs(u):
        color[u] = 1
        path.append(u)
        for v in sorted(adj[u]):
            if color.get(v) == 1:
                cycles.append(path[path.index(v):] + [v])
            elif not color.get(v):
                dfs(v)
        path.pop()
        color[u] = 2

    dfs(g["start"])
    for n in nodes:
        if not color.get(n):
            dfs(n)
    unbounded = [c for c in cycles if not any(x in BOUNDED for x in c)]
    for c in unbounded:
        problems.append("cycle without a bounded node: " + " -> ".join(c))

    static_cycles = []
    color2, path2 = {}, []

    def dfs2(u):
        color2[u] = 1
        path2.append(u)
        for v in sorted(adj_static[u] - {d for d in dyn.get(u, set())}):
            if color2.get(v) == 1:
                static_cycles.append(path2[path2.index(v):] + [v])
            elif not color2.get(v):
                dfs2(v)
        path2.pop()
        color2[u] = 2

    for n in nodes:
        if not color2.get(n):
            dfs2(n)
    for c in static_cycles:
        problems.append("DECLARED (static) cycle, the validator rejects these: " + " -> ".join(c))

    mli = g.get("settings", {}).get("max_loop_iterations")
    if not mli:
        problems.append("settings.max_loop_iterations missing or 0 (engine backstop required)")
    ends = [n for n, v in nodes.items() if v["type"] == "end"]
    unreachable = sorted(n for n in nodes if n not in color)
    print(f"nodes: {len(nodes)}  edges: {len(edges)} (dynamic {sum(len(v) for v in dyn.values())})  ends: {len(ends)}")
    print(f"max_loop_iterations: {mli}  timeout: {g.get('settings', {}).get('timeout')}")
    print(f"cycles (static+dynamic): {len(cycles)}; every one passes a bounded node: {not unbounded}")
    for c in cycles:
        anchors = [x for x in c if x in BOUNDED] or ["NONE"]
        print("  -", " -> ".join(c), "  [bounds: " + "; ".join(f"{a}: {BOUNDED.get(a, '?')}" for a in anchors) + "]")
    print(f"unreachable even with dynamic edges: {unreachable or 'none'}")
    print("PROBLEMS:", problems or "none")
    sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main()
