#!/usr/bin/env python3
"""Fan-out source for context loading.

Has no logic of its own. Exists so the static `next: [plan, knowledge_lookup]`
list on this node fans out into two parallel branches (the LLM planner and
the RAG knowledge lookup) as a single super-step. The validator requires
declared parallel-branch script outputs, so we emit an empty JSON object
explicitly here.

Fail-safe: the node reads no state and computes nothing, so its
happy-path output IS the sane degraded output — the guard emits the same
empty object on any crash. No PIPELINE-FAULT is recorded: appending one
would require reading state (the very thing that crashed) or clobbering
faults recorded upstream by parse_request, and a crash here loses nothing.
"""
import json


def main():
    print(json.dumps({}))


try:
    main()
except Exception:  # noqa: BLE001 — the fan-out source must never crash the graph
    print(json.dumps({}))
