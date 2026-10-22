#!/usr/bin/env python3
"""Measure retrieval of sampled, indexed addresses with the existing regression runner."""
import argparse
import collections
import datetime
import hashlib
import json
import os
from pathlib import Path
import random
import re
import subprocess
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
RADII = (25, 50, 100)
METRICS = [f"top{k}_fields" for k in (1, 5)] + [f"top{k}_{m}m" for k in (1, 5) for m in RADII]
SUFFIXES = {"street": "St", "road": "Rd", "avenue": "Ave", "boulevard": "Blvd", "drive": "Dr", "lane": "Ln", "court": "Ct", "place": "Pl", "terrace": "Tce", "highway": "Hwy", "parkway": "Pkwy", "close": "Cl"}


def freeform(address, street=None, number=None):
    street = street or address["street"]
    number = number or address["number"]
    house = f"{number} {street}"
    if address["unit"]:
        unit = address["unit"]
        if not re.match(r"^(?:unit|apt|apartment|suite|ste|flat|rm|room)\b", unit, flags=re.I):
            unit = "Unit " + unit
        house = f"{unit}, {house}"
    return ", ".join(p for p in (house, address["city"], address["state"], address["postcode"]) if p)


def checks(address):
    fields = {"address.house_number": address["number"], "address.road": address["street"], "address.country_code": address["country"]}
    if address["unit"]:
        fields["address.unit"] = address["unit"]
    if address["postcode"]:
        fields["address.postcode"] = address["postcode"]
    normalizers = {field: ["trim", "toUpperCase"] for field in fields}
    normalizers["address.road"] += ["abbreviateStreetSuffixes", "abbreviateDirectionals", "removeOrdinals", "stripPunctuation"]
    if "address.postcode" in fields:
        normalizers["address.postcode"] += ["stripPunctuation"]
    result = []
    for metric in METRICS:
        limit = 1 if metric.startswith("top1_") else 5
        check = {"kind": "result_matches_within", "limit": limit, "fields": fields, "normalizers": normalizers}
        if not metric.endswith("fields"):
            check["coordinates"] = {"lat": address["lat"], "lon": address["lon"], "max_m": int(metric.split("_")[1][:-1])}
        result.append(check)
    return result


def make_cases(addresses):
    cases = []
    extra_counts = collections.Counter()
    shuffled = list(enumerate(addresses))
    random.Random(0xADD2E55).shuffle(shuffled)
    for index, address in shuffled:
        cc = address["country"]
        common = {"country_code": cc, "limit": "5"}
        structured = {"street": address["street"], "housenumber": address["number"],
                      "city": address["city"], "state": address["state"], "postcode": address["postcode"], "unit": address["unit"]}
        variants = [("structured", {k: v for k, v in structured.items() if v}), ("freeform", {"q": freeform(address)})]
        abbreviated = re.sub(r"\b(" + "|".join(SUFFIXES) + r")\b", lambda m: SUFFIXES[m[0].lower()], address["street"], flags=re.I)
        if abbreviated != address["street"] and extra_counts[(cc, "abbreviation")] < 100:
            variants.append(("abbreviation", {"q": freeform(address, street=abbreviated)}))
        word = next((m for m in re.finditer(r"\b[A-Za-z]{5,}\b", address["street"]) if m[0].lower() not in SUFFIXES), None)
        if word and extra_counts[(cc, "typo")] < 100:
            pos = word.start() + len(word[0]) // 2
            typo = address["street"][:pos] + address["street"][pos + 1:]
            variants.append(("typo", {"q": freeform(address, street=typo)}))
        for variant, query in variants:
            extra_counts[(cc, variant)] += 1
            cases.append({"id": f"house-{cc.lower()}-{index:05d}-{variant}", "tags": ["house-number", cc, variant, address["stratum"]],
                          "reference": address, "request": {"path": "/search", "query": {**common, **query}},
                          "expect": {"status": 200, "checks": checks(address)}})
    return cases


def summarize(cases, reports):
    counts = collections.defaultdict(collections.Counter)
    failures = []
    for case in cases:
        report = reports[case["id"]]
        cc, variant, stratum = case["tags"][1:]
        outcomes = report["check_results"]
        if outcomes and len(outcomes) != len(METRICS):
            raise ValueError("unexpected runner check count")
        first = report.get("first_result") or {}
        has_number = bool((first.get("address") or {}).get("house_number"))
        category = "number_returned" if has_number else "street_fallback" if first.get("kind") == 2 else "other_result" if first else "no_result"
        for group in ["all", f"country/{cc}", f"variant/{variant}", f"stratum/{stratum}", f"{cc}/{variant}", f"{variant}/{stratum}"]:
            bucket = counts[group]
            bucket["total"] += 1
            bucket[category] += 1
            bucket["http_errors"] += report.get("http_status") != 200
            for metric, passed in zip(METRICS, outcomes):
                bucket[metric] += bool(passed) and report.get("http_status") == 200
        if report.get("http_status") != 200 or not outcomes or not outcomes[METRICS.index("top1_50m")]:
            failures.append({"id": case["id"], "variant": variant, "reference": case["reference"], "request": case["request"],
                             "first_result": first, "failures": report["failures"]})
    summary = {group: {"total": c["total"], "outcomes": {k: c[k] for k in ["number_returned", "street_fallback", "other_result", "no_result", "http_errors"]},
                       "metrics": {metric: {"passed": c[metric], "rate": c[metric] / c["total"]} for metric in METRICS}}
               for group, c in sorted(counts.items())}
    return summary, failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--index", type=Path, default=ROOT / "data/index")
    parser.add_argument("--sample", type=int, default=1000, help="unique indexed addresses per country")
    parser.add_argument("--port", type=int, default=13591)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    if not 1 <= args.sample <= 100000:
        parser.error("--sample must be between 1 and 100000")
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    out = args.out or ROOT / "tests/regression/reports" / f"house-numbers-{stamp}"
    out.mkdir(parents=True, exist_ok=False)
    binaries = {name: ROOT / "target/release" / name for name in ["sample-addresses", "query-server", "regression-runner"]}
    for binary in binaries.values():
        if not binary.is_file():
            raise RuntimeError("Missing release binary; run make bench-house-numbers")
    print(f"Sampling {args.sample} addresses per country into {out}", flush=True)
    with (out / "addresses.json").open("w") as output:
        subprocess.run([str(binaries["sample-addresses"]), str(args.index), str(args.sample)], stdout=output, check=True)
    addresses = json.loads((out / "addresses.json").read_text())
    countries = sorted({a["country"] for a in addresses})
    cases = make_cases(addresses)
    corpus = {"name": "house-number-retrieval", "description": "Indexed-data retrieval; not independent address coverage. No coordinate bias. All fields and coordinates must match on one result.", "cases": cases}
    (out / "corpus.json").write_text(json.dumps(corpus, separators=(",", ":")) + "\n")
    base = f"http://127.0.0.1:{args.port}"
    env = dict(os.environ, GEOCODER_GRPC_ADDR="127.0.0.1:0", RUST_LOG="error")
    with (out / "server.log").open("w") as log:
        server = subprocess.Popen([str(binaries["query-server"]), str(args.index), f"127.0.0.1:{args.port}"], stdout=log, stderr=subprocess.STDOUT, env=env)
        try:
            for _ in range(150):
                if server.poll() is not None:
                    raise RuntimeError(f"server exited; see {out / 'server.log'}")
                try:
                    with urllib.request.urlopen(base + "/healthz/ready", timeout=1):
                        break
                except (urllib.error.URLError, TimeoutError):
                    time.sleep(0.2)
            else:
                raise RuntimeError("server did not become ready")
            with urllib.request.urlopen(base + "/healthz/indexes", timeout=5) as response:
                health = json.load(response)
            loaded = {cc.upper() for cc in health["indexes"]["forward"]["countries"]}
            if set(countries) != loaded:
                raise RuntimeError(f"sampled countries {countries} differ from served countries {loaded}")
            print(f"Running {len(cases)} searches without location hints", flush=True)
            with (out / "runner.log").open("w") as runner_log:
                proc = subprocess.run([str(binaries["regression-runner"]), "--corpus", str(out / "corpus.json"), "--base-url", base,
                                       "--report", str(out / "results.json"), "--quiet"], stdout=runner_log, stderr=subprocess.STDOUT)
            if proc.returncode not in (0, 1):
                raise RuntimeError(f"regression runner failed; see {out / 'runner.log'}")
        finally:
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()
    report = json.loads((out / "results.json").read_text())
    reports = {r["id"]: r for r in report["cases"]}
    if len(reports) != len(cases):
        raise RuntimeError("runner did not report every case")
    summary, failures = summarize(cases, reports)
    metadata = {"index": str(args.index.resolve()), "sample_per_country": args.sample, "unique_addresses": len(addresses), "search_requests": len(cases),
                "git_head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
                "git_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True)),
                "server_sha256": hashlib.sha256(binaries["query-server"].read_bytes()).hexdigest(),
                "forward_manifest": json.loads((args.index / "manifest_forward.json").read_text()), "health": health,
                "sampling": "Fixed seed; max(1000, 20 times sample size) candidate documents per country, deduplicated by address identity. Up to 10% each units, suffixes and numeric ranges; remainder filled from available strata. Sampling is weighted by stored address documents, not population or urban/rural labels.",
                "limitations": ["Measures indexed-data retrieval, not independent completeness or production traffic.", "Strict stored house-number/unit identity; range records are queried as recorded, not used to infer individual house locations.", "Coordinate and field matches must occur on the same result. Interpolation is not performed by this search path.", "Number-returned outcome is not necessarily the correct property; use the match metrics for correctness.", "Strict street-label checks can reject equivalent spellings or French street-type placement; failures need triage before interpreting them as wrong properties."]}
    (out / "summary.json").write_text(json.dumps({**metadata, "groups": summary}, indent=2) + "\n")
    (out / "failures.json").write_text(json.dumps(failures, indent=2) + "\n")
    for group in ["variant/structured", "variant/freeform", "variant/abbreviation", "variant/typo"]:
        if group in summary:
            s = summary[group]
            print(f"{group}: {s['total']} searches; first within 50 m {s['metrics']['top1_50m']['rate']:.2%}; top five {s['metrics']['top5_50m']['rate']:.2%}")
    print(f"Report: {out / 'summary.json'}")


if __name__ == "__main__":
    main()
