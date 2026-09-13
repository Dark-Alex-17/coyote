#!/usr/bin/env python3
"""Check that the sources cited in the research report are reachable.

Scans the final report for URLs and DOIs, probes each with a HEAD
request, and writes a `source_check` summary into state so the human
reviewer sees broken citations at the approval step.

Times out per request so a slow source cannot stall the graph.

Pipeline-notes fold: end-node templates render unconditionally, so the
conditional "## Pipeline notes" section of the accepted report is built
HERE — the last stop before the approval gate on every accepted path,
including reflexion and feedback re-loops. `pipeline_notes` is the
rendered section when `pipeline_faults` is non-empty and the empty
string otherwise, keeping the happy-path `{{report}}{{pipeline_notes}}`
output byte-identical to the pre-hardening `{{report}}`.

Fail-safe (R3): per-URL probes were already guarded; the top level now
is too. On a crash the summary says sources were NOT checked, a
PIPELINE-FAULT is recorded (best-effort preserving existing faults), and
pipeline_notes still renders — the approval gate remains the backstop.
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


def safe_faults():
    try:
        state = load_state()
        return [f for f in (state.get("pipeline_faults") or []) if isinstance(f, str)]
    except Exception:  # noqa: BLE001 — best-effort preservation only
        return []


def render_notes(faults):
    entries = [f for f in (faults or []) if isinstance(f, str) and f.strip()]
    if not entries:
        return ""
    return "\n\n## Pipeline notes\n\n" + "\n".join(f"- {e}" for e in entries)


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

    notes = render_notes(state.get("pipeline_faults"))
    print(json.dumps({"source_check": summary, "pipeline_notes": notes}))


try:
    main()
except Exception as e:  # noqa: BLE001 — a crashed source check must degrade, not die
    faults = safe_faults()
    faults.append(
        f"PIPELINE-FAULT: source verification crashed — {e}; cited sources "
        "were not checked for reachability"
    )
    print(
        json.dumps(
            {
                "source_check": (
                    "Source verification crashed (pipeline fault) — cited "
                    "sources were NOT checked for reachability."
                ),
                "pipeline_notes": render_notes(faults),
                "pipeline_faults": faults,
            }
        )
    )
