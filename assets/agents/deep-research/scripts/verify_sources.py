#!/usr/bin/env python3
"""Check that the sources cited in the research report are reachable.

Scans the final report for URLs and DOIs, probes each with a HEAD
request, and writes a `source_check` summary into state so the human
reviewer sees broken citations at the approval step.

Times out per request so a slow source cannot stall the graph.
"""
import json
import os
import re
import urllib.error
import urllib.request

DOI_RE = re.compile(r"\b(10\.\d{4,9}/[-._;()/:A-Z0-9]+)", re.IGNORECASE)
URL_RE = re.compile(r"https?://[^\s)\]\}\"'>]+")


def load_state():
    path = os.environ.get("GRAPH_STATE_FILE")
    if path:
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def reachable(url, timeout=5.0):
    req = urllib.request.Request(url, method="HEAD")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return 200 <= resp.status < 400
    except urllib.error.HTTPError as e:
        return 200 <= e.code < 400
    except Exception:
        return False


def main():
    state = load_state()
    report = state.get("report") or ""

    urls = sorted({u.rstrip(".,;)") for u in URL_RE.findall(report)})
    dois = sorted(set(DOI_RE.findall(report)))

    results = []
    for url in urls:
        ok = reachable(url)
        results.append(f"  {'OK' if ok else 'UNREACHABLE'}  {url}")
    for doi in dois:
        url = f"https://doi.org/{doi}"
        if url in urls:
            continue
        ok = reachable(url)
        results.append(f"  {'OK' if ok else 'UNREACHABLE'}  DOI {doi} ({url})")

    if not results:
        summary = "No web sources were cited in the report."
    else:
        summary = (
            f"Source reachability ({len(results)} checked):\n"
            + "\n".join(results)
        )

    print(json.dumps({"source_check": summary}))


if __name__ == "__main__":
    main()
