# Build & deploy on AWS

End-to-end path from a fresh clone to a production geocoder behind an
internal load balancer. This doc focuses on the AWS-specific pieces;
for architecture, see [`ARCHITECTURE.md`](ARCHITECTURE.md). For the
AMI builder specifically, see [`packer/README.md`](packer/README.md).

**Already running Kubernetes?** See
[`docs/kubernetes-deployment.md`](docs/kubernetes-deployment.md) for
the EBS-snapshot-per-pod pattern that slots into existing EKS / GKE
infrastructure without running an AMI pipeline.

## Architecture at a glance

```
           ┌──────────────┐         ┌───────────────────────┐
clients ─▶ │  Internal    │ ─────▶  │  Auto Scaling Group   │
(VPC)      │  ALB         │   443   │  • t3.xlarge / c6i.*  │
           │  (HTTPS)     │   →     │  • AMI: geocoder-…    │
           └──────────────┘  3000   │  • /var/lib/geocoder  │
                 ▲                  │    /index (mmap'd)    │
                 │                  └───────────────────────┘
                 │                              ▲
                 │                              │ (first-boot only,
           ┌──────────────┐                     │  baked into AMI)
           │ ACM cert     │                     │
           │ (internal    │                     │
           │  domain)     │                ┌────┴────────────┐
           └──────────────┘                │ S3: artifacts   │
                                           │  • index/…      │
                                           │  • bin/…        │
                                           └─────────────────┘
                                                 ▲
                                                 │ packer build / CI
                                                 │
                                          build pipeline
                                          (OSM + G-NAF → index)
```

## Load balancer: ALB or NLB?

**Use an internal ALB.** For a JSON REST + gRPC service behind the LB,
ALB gives you everything that matters:

| Dimension | ALB | NLB | Why ALB for geocoder |
|---|---|---|---|
| HTTP/HTTPS termination | ✅ | TLS passthrough | Offloads TLS from the Rust process |
| HTTP/2 (for gRPC) | ✅ | TLS only | `tonic` needs HTTP/2; ALB's `grpc` target protocol works cleanly |
| Path-based routing | ✅ | ❌ | Useful if you later split `/autocomplete` vs `/search` to different target groups |
| Health checks | HTTP path | TCP/HTTP | Geocoder responds quickly on any endpoint; `/` health-check works |
| Access logs | Per-request detail | Flow logs only | Much better for debugging query patterns |
| Added latency | ~1–3 ms | µs-range | Irrelevant — reverse queries take 100 µs + network |
| WAF integration | ✅ | ❌ | Rate limits / IP allowlists at LB layer |
| Static IPs per AZ | ❌ | ✅ | Rarely needed internally |

NLB only wins when you have one of:

- Non-HTTP protocol (not relevant)
- µs-level latency SLO (geocoder p99 is milliseconds; a few ms of ALB is noise)
- Need for static IPs to pin firewall rules (rarely matters inside a VPC)

If you ever front the geocoder with an NLB, it'd be because some
external dependency (e.g. PrivateLink endpoint service, which only
supports NLB targets) forces your hand. Otherwise: internal ALB.

## Prerequisites

- **AWS account** with permissions to create EC2, AMI, S3, IAM, VPC resources.
- **AWS CLI v2** with credentials configured (`aws configure` or `AWS_PROFILE`).
- **Packer 1.10+** — `brew install hashicorp/tap/packer`.
- **Build machine** — macOS or Linux with:
  - `cargo` (stable Rust)
  - `cmake`, `g++`, `libosmium2-dev`, `libs2-dev`, `libprotozero-dev`,
    `libdeflate-dev` (for fast PBF inflate; libosmium picks it up
    automatically when present), plus `zlib1g-dev`, `libbz2-dev`,
    `libexpat1-dev`, `liblz4-dev` for libosmium's compression
    backends. (Dockerfile-equivalent, or use Docker.)
  - `osmium-tool`, `pyosmium-get-changes` (for index updates)
- **S3 bucket** for artifacts (index files + binary). No special config needed.

## Step 1 — build the index

Run on a beefy machine (16+ GB RAM for AU, 64+ GB for worldwide).
Reuses the existing pipeline scripts:

```bash
# a) download OSM PBF (conditional GET + resumable; re-runs are bandwidth-cheap)
cargo build --release --manifest-path server/Cargo.toml --bin fetch-data
./target/release/fetch-data --region australia --data-dir ./data

# b) build reverse + admin index (C++)
mkdir -p data/index
./build/build-index data/pbf/australia-latest.osm.pbf data/index

# c) (optional) build G-NAF authoritative address index for AU
./target/release/build-gnaf-index data/gnaf data/index

# d) (optional) build OpenAddresses index for worldwide coverage
./target/release/build-openaddresses-index data/openaddresses data/index

# e) build forward (tantivy) + autocomplete (FST) indexes
./target/release/build-forward-index data/index
./target/release/build-autocomplete-fst data/index --layout both
```

At this point `data/index/` contains every mmap file the server needs.
Verify with `du -sh data/index` — AU should be ~2 GB.

## Step 2 — build the binary

```bash
cd server
cargo build --release --bin query-server
# → target/release/query-server
```

Strip it to cut ~30 % of size:

```bash
strip -x target/release/query-server
```

If your build host architecture differs from the target AMI
(`x86_64` amd64 by default), build via Docker or a remote amd64 host.
The Packer config targets `x86_64`; for arm64 instances, change the
`architecture = "arm64"` filter in `packer/geocoder.pkr.hcl` and
cross-compile the binary accordingly.

## Step 3 — upload artifacts to S3

```bash
# Pick a versioned path so rolling back is trivial.
VERSION=$(date +%Y%m%d-%H%M%S)
BUCKET=my-geocoder-artifacts

# Index (many files, ~2 GB for AU)
aws s3 sync data/index s3://$BUCKET/index/au-$VERSION/ --delete

# Binary (single file)
aws s3 cp server/target/release/query-server \
  s3://$BUCKET/bin/query-server-$VERSION

# Tag "latest" for convenience — or keep fully versioned URIs.
aws s3 cp --recursive s3://$BUCKET/index/au-$VERSION/ s3://$BUCKET/index/au-latest/
```

## Step 4 — build the AMI

Full workflow documented in [`packer/README.md`](packer/README.md). TL;DR:

```bash
cp packer/example.pkrvars.hcl packer/prod.pkrvars.hcl
# Edit prod.pkrvars.hcl: bucket paths, build region, copy regions,
# build-side IAM instance profile name.
make ami-init
make ami PKRVARS=packer/prod.pkrvars.hcl
```

Output: AMI IDs printed per region. Capture them for the next step.

## Step 5 — infrastructure

Required AWS resources. This doc describes them in prose; wire them
up in Terraform / CDK / CloudFormation / Pulumi per house style.

### 5.1 Networking

- **VPC**: existing VPC is fine. Needs ≥ 2 private subnets in distinct
  AZs for ASG and ALB.
- **Private subnets** for the instances; the ALB spans them in `internal`
  scheme.

### 5.2 Security groups

- **ALB SG** (`geocoder-alb-sg`):
  - Inbound: TCP 443 from the client CIDRs (other VPC subnets,
    peered VPCs, on-prem via Direct Connect / VPN).
  - Outbound: TCP 3000 to the instance SG.
- **Instance SG** (`geocoder-instance-sg`):
  - Inbound: TCP 3000 from `geocoder-alb-sg` only. No direct
    client access to instances.
  - Outbound: minimal. No S3 access needed post-boot since the index
    is baked into the AMI. Permit 443 to `s3.${region}.amazonaws.com`
    only if you enable hot-reload mode.

### 5.3 IAM

- **Packer build instance profile** (used once per AMI build):
  - `s3:GetObject` on `arn:aws:s3:::$BUCKET/index/…/*`,
    `arn:aws:s3:::$BUCKET/bin/…`
  - `s3:ListBucket` on `arn:aws:s3:::$BUCKET` (needed by `aws s3 sync`)
- **Runtime instance profile**: empty for the baked-AMI flow. Add
  `s3:GetObject` on the index prefix only if you run the hot-reload
  pattern (see "Updating the index" below).

### 5.4 Target group

- **Protocol**: HTTP (TLS terminated at the ALB).
- **Port**: 3000.
- **Target type**: `instance` (for ASG registration).
- **Health check**:
  - **Path**: `/healthz` (200 + `{"status":"ok"}` when alive).
  - **Success codes**: `200`.
  - For granular routing (e.g. target-group-per-country), a richer
    `/healthz/indexes` endpoint reports which optional indexes and
    ISO 3166-1 alpha-2 codes are loaded on each instance. Use it to
    build country-aware listener rules, or just for diagnostics.
- **Deregistration delay**: 30 s — queries complete fast, no reason to
  wait longer.

If you add gRPC, create a second target group with:
- **Protocol version**: `gRPC`
- **Port**: 3001
- **Health check grpc method**: depends on what you surface; the
  reflection service if enabled.

### 5.5 Listener

- **HTTPS 443** on the ALB.
- **ACM certificate** for your internal domain
  (e.g. `geocoder.internal.example.com`).
- **Default action**: forward to the geocoder target group.
- **TLS policy**: `ELBSecurityPolicy-TLS13-1-2-2021-06` (modern).

### 5.6 Launch template + ASG

- **Launch template** references the Packer-built AMI ID.
- **Instance type**: `c6i.large` minimum for AU (2 GB mmap working
  set + overhead). `c6i.xlarge` to `c6i.2xlarge` for worldwide. For
  sustained high QPS, prefer instance types with local NVMe to absorb
  mmap page cache pressure — `m6id` / `c6id` families.
- **IMDSv2**: required (hop limit 2, tokens required).
- **User data**: empty. Everything is baked into the AMI; systemd
  starts the service on boot.
- **ASG**:
  - Min size = desired = 2 (one per AZ).
  - Max size = 4–10 depending on traffic.
  - Registers with the target group by ID.
  - Health check type: `ELB` (uses the target-group health to replace
    unhealthy hosts).
  - Rolling update strategy via Instance Refresh:
    - MinHealthyPercentage: 90 %
    - InstanceWarmup: 60 s
    - Checkpoints: optional, useful for big fleets.

### 5.7 Route 53 (optional)

Internal private hosted zone with an alias record pointing
`geocoder.internal.example.com` → the ALB's DNS name.

## Step 6 — first deploy

1. Run `make ami PKRVARS=…` → note the AMI IDs per region.
2. Update the launch template's `image-id` to the new AMI.
3. Trigger an Instance Refresh on the ASG, or scale-out + scale-in
   manually the first time.
4. Test:
   ```bash
   curl -s 'https://geocoder.internal.example.com/reverse?lat=-33.8&lon=151.2' | jq .
   ```

## Updating the index

Two patterns, each appropriate to different operations:

### Pattern A — immutable, one AMI per index version

1. Rebuild the index (`scripts/update-index.sh` or full rebuild).
2. Upload to a *new* versioned S3 prefix.
3. Build a new AMI pointing at that prefix.
4. Update the launch template, trigger ASG Instance Refresh.

This is the "cloud-native" pattern. No drift, easy to roll back
(previous AMI is still registered). Cost: one AMI build per update.
Recommended for small, infrequent updates and regulated environments.

### Pattern B — hot reload in place

1. Rebuild the index on the instance (or sync from S3 to a staging dir).
2. `mv` the staging dir over `/var/lib/geocoder/index` atomically.
3. `touch /var/lib/geocoder/index/reload.marker`.
4. The service's background reloader (enabled via
   `GEOCODER_RELOAD_MARKER` in `/etc/default/geocoder`) notices the
   `mtime` change and atomically swaps the in-process `ArcSwap<Index>`
   — no restart, no dropped connections.

This is the "update daily from OSM diffs" pattern. Lower cost, faster
turnaround, but instances drift from the AMI. Requires the runtime
instance profile to have `s3:GetObject` on the index prefix.

**Pick one and stick with it.** Mixing the two causes the AMI's
baked-in index to disagree with what's running in memory after a hot
reload, making debugging miserable.

## Operations

### Sinks at a glance

The query server has two **independent** observability sinks. They never
duplicate data, and disabling one does not affect the other:

| Sink                       | Carries           | Format                       | Default in prod | Toggle                                          |
|----------------------------|-------------------|------------------------------|-----------------|-------------------------------------------------|
| **stdout**                 | log events        | NDJSON (one event per line)  | always on       | `RUST_LOG=off` to silence; `GEOCODER_LOG_FORMAT` to change format |
| **OTLP** (gRPC or HTTP)    | distributed spans | OTLP protobuf                | off             | `OTEL_TRACE_ENABLED=true` + `OTEL_EXPORTER_OTLP_ENDPOINT`         |

So in a stock production deploy with no env vars set: **one log format,
one log sink — newline-delimited JSON on stdout**, captured by
`systemd-journald` and shipped wherever you ship journal output. Spans
are still built in-process (so log lines correlate via `spans[]` /
`current_span` fields), but they aren't exported anywhere unless you
opt into OTLP. Setting `OTEL_TRACE_ENABLED=false` does **not** affect
stdout — it only suppresses OTLP export.

### Logs

`query-server` writes NDJSON log lines to **stdout** by default (when
stdout is not a terminal). Each line carries the canonical `tracing`
fields plus any structured key/values attached by the handler:

```json
{"timestamp":"…","level":"INFO","target":"query_server::http","fields":{"message":"request complete","status":200,"duration_ms":1.42},"spans":[{"otel.kind":"server","http.request.method":"GET","url.path":"/reverse","name":"http.request"},{"name":"reverse_geocode","geocoder.lat":-33.85,"geocoder.lon":151.21}]}
```

Aggregators that parse JSON natively (CloudWatch Logs Insights, Loki,
Datadog Logs, Stackdriver) can group by `fields.geocoder.stage`,
`spans[].geocoder.path` etc. without log-parsing rules. On EC2,
`systemd-journald` captures stdout — install the CloudWatch agent in the
AMI and point it at the journal.

Useful environment variables:

| Variable                | Purpose                                                                                                            |
|-------------------------|--------------------------------------------------------------------------------------------------------------------|
| `RUST_LOG`              | EnvFilter directive (e.g. `info,query_server=debug,tower_http=info`). Applied to both sinks — set this to silence. |
| `GEOCODER_LOG_FORMAT`   | `json` (prod, NDJSON), `pretty` (local dev, multi-line ANSI), `compact` (single-line text). Auto-picks JSON when stdout isn't a tty. |

Format note: `json` output is already one event per line — there is no
separate "compact JSON" mode. The `compact` value selects a non-JSON
single-line text format for humans, not a denser JSON variant.

### Tracing (OpenTelemetry)

The query server is fully instrumented with OpenTelemetry spans:
per-request HTTP / gRPC server spans, and a child span tree describing
each pipeline stage (FST fast-path, structured tantivy search, ladder
relaxation rungs, fuzzy fallback, house-number refinement,
reverse-geocode enrichment, IP MaxMind lookup, …). Spans are always
created in-process; export to an OTLP collector is opt-in.

Enable export with:

```bash
OTEL_TRACE_ENABLED=true \
OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector.observability:4317 \
OTEL_SERVICE_NAME=query-server \
OTEL_RESOURCE_ATTRIBUTES=deployment.environment=prod,service.namespace=geocoder \
./query-server data/index
```

| Variable                              | Default                  | Purpose                                                                                              |
|---------------------------------------|--------------------------|------------------------------------------------------------------------------------------------------|
| `OTEL_TRACE_ENABLED`                  | `true` if endpoint set   | Master switch. Set `false` to force-disable OTLP export (logs still go to stdout).                   |
| `OTEL_EXPORTER_OTLP_ENDPOINT`         | `http://localhost:4317`  | Collector endpoint. gRPC port 4317 or HTTP port 4318 — match the protocol below.                    |
| `OTEL_EXPORTER_OTLP_PROTOCOL`         | `grpc`                   | `grpc` / `http/protobuf` / `http/json`.                                                              |
| `OTEL_SERVICE_NAME`                   | `query-server`           | `service.name` resource attribute.                                                                   |
| `OTEL_RESOURCE_ATTRIBUTES`            | unset                    | Comma-separated `key=value` pairs merged into the resource (e.g. `deployment.environment=prod`).     |

Tested against the OpenTelemetry Collector, Tempo, Honeycomb,
Datadog Agent, and Jaeger 2.x — anything that speaks OTLP works.

#### Identifying corrupt index files

Spans and stdout logs both carry attribution fields so an operator can
tie a suspect query (or a suspect result) to a specific file on disk.

**Per-query span attributes** (filterable in any tracing UI):

| Field                              | Recorded on                              | Value                                                                                          |
|------------------------------------|------------------------------------------|------------------------------------------------------------------------------------------------|
| `geocoder.forward.tantivy_dir`     | every `forward.search_*` span            | `tantivy` or `tantivy_<cc>` — the directory tantivy answered from.                             |
| `geocoder.forward.index_variant`   | every `forward.search_*` span            | `default` or `per_country` — distinguishes monolithic vs partitioned even when the dir name doesn't. |
| `geocoder.address.source`          | `find_addr_point` span                   | `gnaf`, `open_addresses_<cc>`, `osm_addr_points`, or `none`.                                   |
| `geocoder.autocomplete.fst_variant`| `autocomplete` and `search.fst_fast_path`| `unified`, `per_country_<cc>`, `any`, or `none`.                                               |

**Startup file manifest** — one INFO line per loaded file at
`target=query_server::manifest`. Fields: `index`, `path`, `size_bytes`,
`mtime_unix`. Filter the boot logs to enumerate every file the process
mmap'd, with sizes you can diff against an expected manifest:

```bash
journalctl -u geocoder | jq -c 'select(.target=="query_server::manifest")'
# {"index":"reverse","path":"data/index/geo_cells.bin","size_bytes":98765432,"mtime_unix":1714060800,…}
# {"index":"reverse","path":"data/index/strings.bin","size_bytes":482711040,"mtime_unix":1714060800,…}
# {"index":"gnaf","path":"data/index/gnaf_points.bin","size_bytes":1234567,"mtime_unix":1714060800,…}
# {"index":"open_addresses","path":"data/index/oa_us_points.bin","size_bytes":98765432,"mtime_unix":1714060800,…}
# {"index":"forward","dir_name":"tantivy_au","path":"data/index/tantivy_au","size_bytes":654321098,"file_count":42,"mtime_unix":1714060800,…}
# {"index":"autocomplete","path":"data/index/fst_au.fst","size_bytes":12345678,"mtime_unix":1714060800,…}
# {"index":"ip_geo","path":"data/GeoLite2-City.mmdb","size_bytes":76543210,"mtime_unix":1714060800,…}
```

**Investigating a bad result**: pull the trace for the bad request, read
the leaf span's `geocoder.address.source` / `geocoder.forward.tantivy_dir`,
then grep the boot manifest for the same path. A 0-byte file or an
mtime that disagrees with the rest of the index points at the corrupt
artifact.

### Shadow validation against Google's Geocoding API

Optional. A small fraction of `/reverse` and `/search` requests can be
shadowed to Google's Geocoding API for in-production accuracy
measurement. The shadow runs **fire-and-forget on a background
worker** — handler latency is unaffected by Google's response time.
Operators get span attributes + structured stdout log lines they can
aggregate to compute admin-field match rates and forward-search
top-1 distance histograms.

**Cost protection (defence in depth):**

1. Operator master switch (`GOOGLE_GEOCODING_ENABLED`).
2. Probabilistic sample gate (`GOOGLE_GEOCODING_SAMPLE_RATE`, default 0.001).
3. Token-bucket RPS cap (`GOOGLE_GEOCODING_RPS_CAP`, default 4).
4. Hard daily cap with UTC-midnight reset (`GOOGLE_GEOCODING_DAILY_CAP`, default 1000 — well under the $200/month free tier at $5/1000 calls).
5. API-key absence disables the path entirely (no key → no calls, no startup error).

The four are independent — a misconfiguration of any one of them is
caught by the others.

**Failure isolation:** A `REQUEST_DENIED` (bad key, billing problem)
disables shadowing for the rest of the process lifetime, with one
loud error log. `OVER_QUERY_LIMIT` (Google's 429) sets a 30 s
backoff window during which the worker drains the channel without
dispatching. HTTP timeouts and network errors drop the sample with
a deduped warning.

**Master switch truth table** — the same shape as `OTEL_TRACE_ENABLED`,
so an operator who knows one knows both:

| `GOOGLE_GEOCODING_ENABLED` | API key set | Result                                            |
|----------------------------|-------------|---------------------------------------------------|
| unset                      | yes         | enabled (implicit; key presence is the cue)       |
| unset                      | no          | disabled (no key → nothing to do)                 |
| `true`/`1`/`yes`/`on`      | yes         | enabled                                           |
| `true`/`1`/`yes`/`on`      | no          | disabled + WARN (flag on but no key)              |
| `false`/`0`/`no`/`off`/`""`| either      | disabled (operator opt-out)                       |
| garbage value              | either      | disabled + WARN                                   |

The "key set but disabled" path is the headline use case — operators
staging a deploy with the secret already provisioned but choosing not
to spend yet.

**Environment variables:**

| Var                                  | Default                  | Purpose                                                                       |
|--------------------------------------|--------------------------|-------------------------------------------------------------------------------|
| `GOOGLE_GEOCODING_ENABLED`           | unset                    | Operator master on/off, independent of the key. See truth table above.        |
| `GOOGLE_GEOCODING_API_KEY`           | unset                    | Required when enabled. Unset → shadow disabled regardless of the flag.        |
| `GOOGLE_GEOCODING_SAMPLE_RATE`       | `0.001`                  | Probabilistic per-request gate (0.0–1.0).                                     |
| `GOOGLE_GEOCODING_DAILY_CAP`         | `1000`                   | Hard daily ceiling. UTC-midnight reset.                                       |
| `GOOGLE_GEOCODING_RPS_CAP`           | `4`                      | Token-bucket smoothing.                                                       |
| `GOOGLE_GEOCODING_INFLIGHT_CAP`      | `4`                      | Semaphore for concurrent in-flight calls.                                     |
| `GOOGLE_GEOCODING_QUEUE_CAPACITY`    | `256`                    | Bounded mpsc; `try_send` drops past full.                                     |
| `GOOGLE_GEOCODING_TIMEOUT_MS`        | `2000`                   | Per-request timeout.                                                          |
| `GOOGLE_GEOCODING_BACKOFF_SECS`      | `30`                     | OVER_QUERY_LIMIT cooldown.                                                    |

**Comparison output:** every shadow emits one tracing span (target
`query_server::shadow`) plus one structured stdout log line. Span
attributes — useful for OTLP-side aggregations:

```
geocoder.shadow.endpoint            "reverse" | "search"
geocoder.shadow.outcome             "match" | "mismatch" | "zero_results"
                                    | "google_error" | "auth_disabled"
                                    | "daily_cap_hit" | "queue_full"
                                    | "rate_limited" | "timeout"
geocoder.shadow.country.match       bool
geocoder.shadow.state.match         bool
geocoder.shadow.city.match          bool
geocoder.shadow.road.match          bool   (reverse only)
geocoder.shadow.distance_m          f64    (search only — top-1 haversine)
geocoder.shadow.our.country_code    "AU"
geocoder.shadow.google.country_code "AU"
geocoder.shadow.google.status       "OK" | "ZERO_RESULTS" | …
geocoder.shadow.latency_ms          u64    (Google call wall time)
```

For mismatches, the structured log line additionally carries
`our_formatted` + `google_formatted` raw strings so an operator
investigating a known-bad reading can grep for it without joining
two systems.

### Prometheus metrics

Scrape `GET /metrics` for request rate, latency, and shadow-validation
accuracy in the standard Prometheus text-exposition format. The
endpoint is **always** available — no auth, no env-var gate. Operators
gate scraper access at the network layer (security-group / ingress
rule); leaving the path open over the open internet would expose the
country-traffic split.

Sample scrape config:

```yaml
- job_name: geocoder
  metrics_path: /metrics
  scrape_interval: 15s
  static_configs:
    - targets: ['geocoder.internal:3000']
```

**Metric reference:**

| Name                                  | Type      | Labels                  | Notes                                                                  |
|---------------------------------------|-----------|-------------------------|------------------------------------------------------------------------|
| `geocoder_requests_total`             | counter   | endpoint, country       | RPS via PromQL `rate()`. Country is ISO alpha-2 lowercase or `unknown`. |
| `geocoder_request_duration_seconds`   | histogram | endpoint                | Buckets tuned for 0.5 ms–2.5 s range.                                  |
| `geocoder_shadow_outcomes_total`      | counter   | endpoint, outcome       | Shadow validator outcome enum (9 values).                              |
| `geocoder_shadow_match_total`         | counter   | endpoint, axis, result  | Per-axis admin-field match. Axis ∈ {country,state,city,road}; result ∈ {match,mismatch,none}. |
| `geocoder_shadow_distance_meters`     | histogram | endpoint                | /search top-1 distance against Google's top-1 (metres).                |
| `geocoder_shadow_queue_full_total`    | counter   | —                       | Cumulative shadow `try_send` failures. Should stay flat in steady state. |

**Cardinality:** ~7 endpoints × 250 countries (in practice ~10–60 served) = ~1750 series for `requests_total` worst case, ~600 in any realistic deployment. Bounded by the alpha-2 normaliser — anything else collapses to `country="unknown"`.

**Headline PromQL:**

```promql
# Overall RPS
sum(rate(geocoder_requests_total[1m]))

# Per-country RPS, /search only
sum by (country) (rate(geocoder_requests_total{endpoint="search"}[1m]))

# p99 latency per endpoint
histogram_quantile(0.99, sum by (le, endpoint) (rate(geocoder_request_duration_seconds_bucket[5m])))

# Shadow accuracy (1 - mismatch share) per endpoint
1 - (
  sum by (endpoint) (rate(geocoder_shadow_outcomes_total{outcome="mismatch"}[5m]))
  / sum by (endpoint) (rate(geocoder_shadow_outcomes_total{outcome=~"match|mismatch"}[5m]))
)

# /search top-1 distance p95 against Google
histogram_quantile(0.95,
  sum by (le) (rate(geocoder_shadow_distance_meters_bucket{endpoint="search"}[5m])))

# Per-axis match rate (e.g. "how often does our state agree with Google's?")
sum by (axis) (rate(geocoder_shadow_match_total{result="match"}[5m]))
  / sum by (axis) (rate(geocoder_shadow_match_total{result=~"match|mismatch"}[5m]))
```

**Why counters, not RPS gauges:** Prometheus convention stores rates as
counters and the dashboard computes RPS via `rate(metric[1m])` at
query time. Storing an in-process gauge of "current RPS" would
require a sliding-window estimator and would disagree with whichever
window the dashboard chose anyway. The counter form composes correctly
with PromQL aggregations and recording rules.

### Same metrics over OTLP

The same six series are also pushed to an OTel collector when
`OTEL_METRICS_ENABLED=true` (or `OTEL_EXPORTER_OTLP_ENDPOINT` is set
and the master switch is unset). One in-process meter provider feeds
both surfaces — the Prometheus `/metrics` text format **and** the
OTLP push — so dashboards aggregating across the two backends can't
disagree.

| Variable                          | Default       | Effect                                                           |
|-----------------------------------|---------------|------------------------------------------------------------------|
| `OTEL_METRICS_ENABLED`            | follows endpoint | Operator on/off, identical truth-matrix to `OTEL_TRACE_ENABLED`. Unset → enabled iff endpoint is set. |
| `OTEL_METRIC_EXPORT_INTERVAL`     | `30000` (ms)  | Periodic export interval. Clamped to `[1000, 300000]`. Spec-named per OTel.                          |
| `OTEL_EXPORTER_OTLP_ENDPOINT`     | `http://localhost:4317` | Shared with traces — same wire path.                                                                 |
| `OTEL_EXPORTER_OTLP_PROTOCOL`     | `grpc`        | Shared with traces. Per-signal protocol override is out of scope.                                    |

The push is independent from the scrape: a dead OTel collector keeps
`/metrics` working unchanged, observation latency stays sub-millisecond,
and OTel's internal-logs warnings about export failures get
swallowed by the dedup filter so stdout doesn't flood under sustained
outage.

Smoke test:

```bash
docker run --rm -p 4317:4317 -p 4318:4318 otel/opentelemetry-collector-contrib --config <(cat <<EOF
receivers:
  otlp:
    protocols:
      grpc: {}
      http: {}
exporters:
  debug: { verbosity: detailed }
service:
  pipelines:
    metrics:
      receivers: [otlp]
      exporters: [debug]
EOF
)

OTEL_TRACE_ENABLED=true \
OTEL_METRICS_ENABLED=true \
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 \
OTEL_METRIC_EXPORT_INTERVAL=5000 \
./target/release/query-server data/index &

# Within 5–10 s, the collector logs both traces and metrics. The
# Prometheus path keeps working in parallel.
curl -s 'http://localhost:3000/reverse?lat=-33.8568&lon=151.2153' >/dev/null
curl -s http://localhost:3000/metrics | grep geocoder_requests_total
```

### Metrics (legacy ALB-side)

The service doesn't export Prometheus / CloudWatch metrics today.
Minimum viable metrics come from the ALB:

- `TargetResponseTime` (p50/p90/p99)
- `HTTPCode_Target_2XX_Count` vs `5XX_Count`
- `HealthyHostCount`
- `RequestCountPerTarget`

Set alarms on:

- p99 latency > 100 ms for 5 min (something is paging in mmaps
  constantly, or the instance is undersized)
- 5XX rate > 1 % for 5 min
- Healthy host count < desired for 10 min

### Troubleshooting

| Symptom | First check |
|---|---|
| 503 from ALB | Healthy host count. Usually a new deploy where instances aren't yet passing the health check. |
| 5XX spikes on cold boot | mmap page-in latency. Instance type too small, or kernel page cache evicted by noisy neighbour. |
| Geocoder returns wrong suburb for a known address | Index drift. Check `/var/lib/geocoder/index` mtime vs the AMI build time. |
| Service won't start after AMI refresh | `journalctl -u geocoder`. Typical: IAM / file-permission mismatch on `/var/lib/geocoder/index`. |

## Cost sketch (ap-southeast-2, indicative)

Assuming AU-only coverage, ~10 QPS steady-state:

| Resource | Monthly |
|---|---|
| 2× `c6i.large` (on-demand, 730 hrs) | ~$130 |
| ALB (730 hrs + small LCUs) | ~$25 |
| EBS gp3 root (20 GB × 2) | ~$4 |
| S3 artifacts (5 GB) | ~$0.15 |
| Data transfer (intra-AZ, minimal) | ~$5 |
| **Total** | **~$165** |

At 10× the traffic: same infra, LCU goes up a few dollars, same order
of magnitude. The geocoder is CPU-light and mmap-heavy; you're paying
for always-on compute + the LB, not per-query costs.

For reserved / savings plan pricing, knock ~30–40 % off the compute line.

## Future work

- Native S3 fetch tool (via `object_store`) to replace `aws s3 sync`
  in the Packer build and enable integrity checks against a
  sha256 manifest file.
- CloudWatch metric exporter (or Prometheus endpoint) so ASG scaling
  policies can key off `request_count_per_instance` rather than just
  ALB-derived metrics.
