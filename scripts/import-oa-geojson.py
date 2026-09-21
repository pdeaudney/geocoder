#!/usr/bin/env python3
"""Download selected public OpenAddresses GeoJSON sources as builder CSVs.

Usage: python3 scripts/import-oa-geojson.py us/ca/san_francisco ca/on/city_of_toronto
Source licenses vary; select sources whose terms fit your deployment.
"""

import argparse
import csv
import gzip
import json
import math
import re
import subprocess
from pathlib import Path
from urllib.parse import quote


HEADER = ["LON", "LAT", "NUMBER", "STREET", "UNIT", "CITY", "DISTRICT", "REGION", "POSTCODE", "ID", "HASH"]


def clean(value):
    # Keep address labels on one line for display and text indexing.
    return " ".join(str(value or "").split())


def curl_json(url):
    return json.loads(subprocess.check_output(["curl", "-fLsS", "--retry", "2", url]))


def import_source(source, root):
    if not re.fullmatch(r"[a-z]{2}(?:/[a-z0-9_]+)+", source):
        raise ValueError(f"invalid OpenAddresses source path: {source}")
    rows = curl_json(
        "https://batch.openaddresses.io/api/data?layer=addresses&source="
        + quote(source, safe="")
    )
    row = next((item for item in rows if item.get("source") == source), None)
    if not row or not row.get("job") or not row.get("output", {}).get("output"):
        raise RuntimeError(f"no current address output for {source}")

    job = row["job"]
    job_info = curl_json(f"https://batch.openaddresses.io/api/job/{job}")
    slug = source.replace("/", "_")
    country = source.split("/", 1)[0]
    csv_path = root / country / f"{slug}.csv"
    meta_path = csv_path.with_suffix(".source.json")
    if csv_path.exists() and meta_path.exists():
        old = json.loads(meta_path.read_text())
        if old.get("job") == job:
            old["license"] = job_info.get("license")
            meta_path.write_text(json.dumps(old, indent=2) + "\n")
            print(f"unchanged {source}: {old.get('kept')} rows")
            return

    job_base = job_info.get("pmtiles_url", "").rsplit("/", 1)[0]
    job_base = job_base or f"https://v2.openaddresses.io/batch-prod/job/{job}"
    url = f"{job_base}/source.geojson.gz"
    raw_path = root / "raw" / f"{slug}.geojson.gz"
    raw_path.parent.mkdir(parents=True, exist_ok=True)
    csv_path.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["curl", "-fLsS", "--retry", "2", "--max-time", "900", "-o", str(raw_path), url],
        check=True,
    )

    kept = skipped = 0
    partial = csv_path.with_suffix(".csv.partial")
    with gzip.open(raw_path, "rt") as source_file, partial.open("w", newline="") as output:
        writer = csv.writer(output)
        writer.writerow(HEADER)
        for line in source_file:
            feature = json.loads(line)
            props = feature.get("properties") or {}
            geometry = feature.get("geometry") or {}
            coords = geometry.get("coordinates") or []
            number = clean(props.get("number"))
            street = clean(props.get("street"))
            if (
                geometry.get("type") != "Point"
                or len(coords) < 2
                or not number
                or number == "0"
                or not street
            ):
                skipped += 1
                continue
            lon, lat = coords[:2]
            if not (
                isinstance(lat, (int, float))
                and isinstance(lon, (int, float))
                and math.isfinite(lat)
                and math.isfinite(lon)
                and -90 <= lat <= 90
                and -180 <= lon <= 180
                and (lat != 0 or lon != 0)
            ):
                skipped += 1
                continue
            writer.writerow(
                [lon, lat, number, street]
                + [clean(props.get(key)) for key in ("unit", "city", "district", "region", "postcode", "id", "hash")]
            )
            kept += 1

    partial.replace(csv_path)
    meta_path.write_text(json.dumps({"source": source, "job": job, "url": url, "license": job_info.get("license"), "kept": kept, "skipped": skipped}, indent=2) + "\n")
    print(f"imported {source}: {kept} rows, {skipped} skipped -> {csv_path}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("sources", nargs="+")
    parser.add_argument("--output", type=Path, default=Path("data/openaddresses"))
    args = parser.parse_args()
    for source in args.sources:
        import_source(source, args.output)


if __name__ == "__main__":
    main()
