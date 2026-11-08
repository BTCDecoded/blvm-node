#!/usr/bin/env python3
"""Score fee-eval JSONL against each block's 5th to 75th weight percentile.

A quote below ``band.p5`` is under, above ``band.p75`` is over, and a quote
on either edge or between them is within. Null quotes and blocks with no band
are not counted. ``native`` is the published table. Each object in ``external``
is scored under its ``estimator`` name (``blend`` and ``blend-min`` when those
tables are present).
"""

import json
import sys
from collections import defaultdict


def classify(quote, band):
    if quote is None or band is None:
        return None
    if quote < band["p5"]:
        return "under"
    if quote > band["p75"]:
        return "over"
    return "within"


def add(counts, key, kind):
    if kind is None:
        return
    counts[key][kind] += 1


def score_quote_map(counts, prefix, quotes, band):
    if not isinstance(quotes, dict):
        return
    values = list(quotes.values())
    if values and all(isinstance(value, dict) for value in values):
        for mode, targets in quotes.items():
            score_quote_map(counts, f"{prefix} {mode}", targets, band)
        return
    for target, quote in quotes.items():
        add(counts, f"{prefix} {target}", classify(quote, band))
        add(counts, prefix, classify(quote, band))


def emit(counts):
    for key in sorted(counts):
        row = counts[key]
        total = row["over"] + row["within"] + row["under"]
        if total == 0:
            continue
        print(
            f"{key}: over {row['over']} within {row['within']} under {row['under']} "
            f"over_rate {row['over'] / total:.4f} "
            f"within_rate {row['within'] / total:.4f} "
            f"under_rate {row['under'] / total:.4f}"
        )


def main(argv):
    if len(argv) != 2:
        print("usage: score-fee-eval.py <eval.jsonl>", file=sys.stderr)
        return 2
    counts = defaultdict(lambda: {"over": 0, "within": 0, "under": 0})
    with open(argv[1], encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            record = json.loads(line)
            band = record.get("band")
            native = record.get("native") or {}
            for mode, views in native.items():
                if not isinstance(views, dict):
                    continue
                for view, targets in views.items():
                    score_quote_map(counts, f"native {mode} {view}", targets, band)
                    for target, quote in (targets or {}).items():
                        add(counts, "native", classify(quote, band))
            for table in record.get("external") or []:
                name = table.get("estimator") or "external"
                score_quote_map(counts, name, table.get("quotes") or {}, band)
    emit(counts)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
