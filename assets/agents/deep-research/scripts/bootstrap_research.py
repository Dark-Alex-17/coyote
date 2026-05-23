#!/usr/bin/env python3
"""Fan-out source for context loading.

Has no logic of its own. Exists so the static `next: [plan, knowledge_lookup]`
list on this node fans out into two parallel branches (the LLM planner and
the RAG knowledge lookup) as a single super-step. The validator requires
declared parallel-branch script outputs, so we emit an empty JSON object
explicitly here.
"""
import json


def main():
    print(json.dumps({}))


if __name__ == "__main__":
    main()
