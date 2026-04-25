# Capacity plan

How to size a fleet for a given expected QPS, and how to know when
to scale. This document is **opinionated about methodology**; the
specific numbers should be re-derived from
[`benchmarks.md`](benchmarks.md) and a load test against your
deployment.

## Resource requirements per instance

These are the floor; bigger instances buy you safety margin under
spike, not steady-state throughput.

| Resource | AU index (~2 GB) | 5-country index (~12 GB) | Worldwide (~45 GB) |
|----------|------------------|---------------------------|---------------------|
| **vCPU** | 2 | 4 | 8 |
| **RAM**  | 8 GB | 16 GB | 64 GB |
| **Disk** | 30 GB SSD | 60 GB SSD | 200 GB SSD |
| **Recommended EC2 type** | `c6i.large` | `c6i.xlarge` | `c6i.2xlarge` or `r6i.2xlarge` |

The dominant cost is RAM, not CPU. The mmap'd index pages into the
kernel page cache on demand; once warm, query latency is bound by
CPU on the S2 cell lookup + polygon point-in-polygon. Below ~2× the
index size in RAM you get a steady stream of page faults under
diverse traffic; below 1× the index size you'll thrash.

`c6i` (Intel Ice Lake) recommended over `c5` because the bigger L3
cache helps the polygon traversal, and over `m6i` because we don't
need the extra RAM:CPU ratio for AU. For multi-country indexes
where RAM > 2× index size starts to bite, switch to `r6i`.

## Throughput per instance — methodology

The Criterion benches in `server/benches/` capture point latency,
not throughput. Throughput per instance depends on:
1. Workload mix (`/reverse` is faster than `/search`; FST fast-path
   in `/search` is ~10× faster than the tantivy ladder).
2. Query distribution (ocean coords resolve in microseconds; dense
   urban with full enrichment in ~1 ms).
3. Concurrency level (axum's default tokio worker count = CPU count).

To derive your number, run a sustained `wrk2` / `vegeta` test
against a warmed instance:

```bash
# Warm the page cache first.
for _ in $(seq 1 100); do
    curl -s http://localhost:3000/reverse?lat=-33.8568&lon=151.2153 >/dev/null
done

# Run the load. -R = constant rate; raise until p99 ≥ SLO.
wrk2 -t4 -c64 -d60s -R5000 \
    --latency \
    -s scripts/wrk-mixed-coords.lua \
    http://localhost:3000/reverse
```

The first rate at which p99 latency exceeds the SLO target (50 ms
for `/reverse`, 100 ms for `/search` — see [`docs/sli-slo.md`](../sli-slo.md))
is the per-instance saturation RPS. **Plan for 50 % of saturation
in steady state** so a host failure doesn't cascade.

### Indicative numbers (re-verify per deployment)

Captured on `c6i.large` against the AU index, mixed-coord workload:

| Metric                       | Indicative value |
|------------------------------|------------------|
| Saturation RPS (p99 < 50 ms) | ~3 500 req/s     |
| Steady-state RPS (50 % cap)  | ~1 750 req/s     |
| RAM at steady state          | ~2.5 GB resident |
| CPU at steady state (50 %)   | ~30 % all cores  |

Sources of variance: workload mix, kernel page-cache warmth, neighbour noise. If your numbers
differ by > 30 % from these, instrument and check the
`tower_http::TraceLayer` per-request span timings — usually the
delta is in p99 outliers from cold-cache requests.

## Autoscaling triggers

Don't autoscale on CPU alone. The query path is mmap-bound; CPU
goes up linearly with traffic but page-cache pressure is the
warning sign for tail latency.

Recommended Auto Scaling Group rules:

| Trigger | Threshold | Action |
|---------|-----------|--------|
| `RequestCountPerTarget` (ALB target) | sustained > 1 000 req/s for 5 min | scale +1 |
| `RequestCountPerTarget` | sustained < 300 req/s for 30 min | scale -1 (down to min) |
| `TargetResponseTime p99` | > 50 ms for 5 min | scale +1 (latency-sensitive) |
| `MemoryUtilization` (CW agent) | > 90 % for 10 min | scale +1 (page-cache thrash) |

Scaling **out** should be aggressive (≤ 5 min reaction); scaling
**in** should be cautious (≥ 30 min cool-down) — a freshly-launched
instance has cold mmap and inflates p99 for the first ~30 s.

Min ASG size = 2 (rolling-deploy availability, AZ tolerance).
Recommended max = 4 × steady-state instance count, capped by burst
budget on Google's free tier if shadow validation is enabled.

## Cost per million queries

Order-of-magnitude figures, AWS ap-southeast-2 on-demand pricing,
2026-04 rates. Re-derive for your region / commitments.

For a single `c6i.large` instance running 24/7 at ~1 750 RPS
steady-state (~50 % of saturation):

| Cost | Per month | Per 1 M queries |
|------|-----------|-----------------|
| EC2 (`c6i.large` on-demand) | ~$65 | $0.014 |
| EBS gp3 (30 GB) | ~$2.40 | $0.0005 |
| ALB (LCU portion) | ~$5–10 | $0.001–0.002 |
| Data transfer (intra-AZ minimal) | negligible | – |
| **Subtotal** | **~$75/month** | **~$0.016 / 1M queries** |

At 1 750 RPS × 30 days = ~4.5 B queries/month, that's effectively
free per query.

Add **shadow validation cost** if enabled:
- Default sample rate 0.1 % × 1 750 RPS = 1.75 shadow calls/s
  → 4.5 M/month, capped by `GOOGLE_GEOCODING_DAILY_CAP` to
  1 000/day = 30 000/month.
- 30 000 calls × $5/1000 = **$150/month** (well under the $200 free tier).

For a 3-instance fleet (HA + 2 × steady-state spike room):
~$225/month infra + $150 shadow = **~$375/month for ~13 B
queries** (once the fleet is fully utilised).

Reserved instances or savings plans drop the EC2 line by 30–40 %.

## When to add another region

Single-region is the default and right for most deployments. Add a
second region when:

- p99 from your client region exceeds the SLO (look at the LB
  access logs' upstream RTT, not the geocoder's response time).
- Regional outage tolerance is contractually required.
- A specific country's data has mass adoption and routing it
  through the same fleet would saturate it (in practice, this
  takes > 10k RPS sustained).

Active-active multi-region is straightforward because the index is
immutable per build: deploy the same AMI in both regions, point a
Route 53 latency-routed alias at both, run independent ASGs. **Cost
roughly doubles**; latency wins are typically 50–150 ms for
cross-Pacific clients.

For active-passive DR (cheaper), keep the standby region's ASG min
size at 0 and rely on Route 53 health-check failover. Cold-start
takes ~5 minutes to reach steady-state RPS while the kernel page
cache warms.
