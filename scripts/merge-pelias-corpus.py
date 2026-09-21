#!/usr/bin/env python3
"""Merge all pelias/acceptance-tests *.json files into one synthetic
corpus so our Rust adapter can filter the whole thing by country in
one pass.

Each merged case retains its original endpoint, ranking threshold, and
expectation metadata so the adapter can reject unsupported semantics.

Test IDs are prefixed with the source filename so collisions across
files don't drop cases.

Usage:
    python3 scripts/merge-pelias-corpus.py \\
        test-data/pelias-acceptance-tests/test_cases \\
        > /tmp/pelias-combined.json
"""
import json
import os
import sys


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: merge-pelias-corpus.py <pelias-test_cases-dir>", file=sys.stderr)
        return 2
    src_dir = sys.argv[1]
    if not os.path.isdir(src_dir):
        print(f"not a dir: {src_dir}", file=sys.stderr)
        return 2

    combined = {"name": "pelias-combined", "tests": []}
    seen_ids = set()
    for fname in sorted(os.listdir(src_dir)):
        if not fname.endswith(".json"):
            continue
        path = os.path.join(src_dir, fname)
        try:
            with open(path) as f:
                d = json.load(f)
        except Exception as e:
            print(f"skip {fname}: {e}", file=sys.stderr)
            continue

        stem = os.path.splitext(fname)[0]
        for t in d.get("tests", []) or []:
            orig = t.get("id", "?")
            new_id = f"{stem}-{orig}"
            if new_id in seen_ids:
                continue
            seen_ids.add(new_id)
            merged = dict(t)
            merged["id"] = new_id
            merged["endpoint"] = t.get("endpoint") or d.get("endpoint") or "search"
            merged["priorityThresh"] = t.get("priorityThresh", d.get("priorityThresh", 1))
            merged["distanceThresh"] = t.get("distanceThresh", d.get("distanceThresh"))
            merged["normalizers"] = t.get("normalizers", d.get("normalizers"))
            merged["weights"] = t.get("weights", d.get("weights"))
            combined["tests"].append(merged)

    json.dump(combined, sys.stdout)
    print(f"merged {len(combined['tests'])} tests from {src_dir}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
