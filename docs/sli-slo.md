# SLIs and SLOs

Service-level indicators (what we measure) and objectives (what we
commit to) for the query server. The values below are **starting
points**, calibrated to the indicative throughput in
[`docs/performance/capacity-plan.md`](performance/capacity-plan.md).
Tighten them once you have a few weeks of production data and a
sense of your real tail latencies.

The numbers here are referenced from
[`docs/alerts/prometheus-alerts.yaml`](alerts/prometheus-alerts.yaml).
**Changing an SLO without updating the alert thresholds is a
configuration drift bug** — the SLO is the contract, the alert is
the enforcement.

## SLI catalogue

Five SLIs cover the headline production behaviour. Each is
expressed as a Prometheus query so the SLO machinery is the same
shape as the alerts.

### 1. Request latency (per endpoint)

```promql
histogram_quantile(0.99,
  sum by (le, endpoint) (rate(geocoder_request_duration_seconds_bucket[5m])))
```

Per-endpoint p99 over a 5-minute window. Pre-aggregated as
`geocoder:request_duration_seconds:p99_5m`.

### 2. Request availability (per endpoint)

```promql
1 - (
  sum by (endpoint) (rate(geocoder_request_5xx_total[5m]))
  /
  sum by (endpoint) (rate(geocoder_requests_total[5m]))
)
```

Successful requests / total. Treats 5xx as failures and 4xx as
client error (not a service failure).

> **Note:** `geocoder_request_5xx_total` is **not yet emitted**.
> Track via the ALB's `HTTPCode_Target_5XX_Count` until the metric
> ships. Issue: TBD.

### 3. Shadow accuracy (per endpoint)

```promql
sum by (endpoint) (rate(geocoder_shadow_outcomes_total{outcome="match"}[5m]))
/
sum by (endpoint) (rate(geocoder_shadow_outcomes_total{outcome=~"match|mismatch"}[5m]))
```

Pre-aggregated as `geocoder:shadow_match_rate_5m`. The denominator
deliberately excludes `zero_results`, `google_error`, `timeout` —
those represent shadow infrastructure issues, not accuracy.

### 4. /search top-1 distance from Google's top-1

```promql
histogram_quantile(0.95,
  sum by (le) (rate(geocoder_shadow_distance_meters_bucket{endpoint="search"}[5m])))
```

p95 distance in metres. Targets calibrate to "within a city block"
for urban searches.

### 5. Index freshness

```promql
(time() - max(geocoder_index_mtime_seconds)) / 86400
```

Days since the index was rebuilt. **Not yet emitted as a metric**;
read from the deploy timestamp / S3 object metadata until it ships.

## SLOs

Four objectives, calibrated to a typical commercial geocoder
deployment. **Dial these to your business reality** — a
no-revenue-impact internal service can run at 99% availability;
a customer-facing API that signs SLAs needs 99.95% or better.

### SLO-1: Request latency

| Endpoint | Target | Window |
|----------|--------|--------|
| `/reverse` | p99 ≤ **50 ms** | 28-day rolling |
| `/search`  | p99 ≤ **100 ms** | 28-day rolling |
| `/autocomplete` | p99 ≤ **30 ms** | 28-day rolling |
| `/geocode/ip` | p99 ≤ **20 ms** | 28-day rolling |
| `/h3` | p99 ≤ **5 ms** | 28-day rolling |
| `/validate` | p99 ≤ **150 ms** | 28-day rolling |

Rationale:
- `/reverse` is mmap + S2 + polygon containment — bound by CPU on
  warm cache. 50 ms is generous for non-warm states.
- `/search` traverses the tantivy ladder + enrichment; 100 ms
  covers the typical fuzzy-fallback worst case.
- `/h3` is pure h3o computation, no I/O — should never hit 5 ms
  unless the runtime is starved.

### SLO-2: Request availability

99.9% successful requests over a 28-day rolling window.

That's an error budget of ~43 minutes/month or ~1 hour/quarter.
Spend it on planned operations (deploys, index rotations, infra
upgrades); a single 5xx storm during peak traffic can burn it
fast.

### SLO-3: Shadow accuracy

| Axis | Target | Window |
|------|--------|--------|
| Country match rate | ≥ **99%** | 7-day rolling |
| State match rate | ≥ **97%** | 7-day rolling |
| City match rate | ≥ **90%** | 7-day rolling |
| `/search` distance p95 | ≤ **500 m** | 7-day rolling |

These reflect what we expect from our index *vs Google* —
not absolute accuracy. Country should be near-perfect; the further
"down" the admin hierarchy you go the noisier the match because
Google's locality definitions differ from OSM's. **A drop in
country match rate is an alarm; a drop in city match rate is a
ticket.**

### SLO-4: Index freshness

Index rebuilt at most **30 days** ago.

OSM data drifts ~1 % per month for active cities. Beyond 30 days
new addresses materially miss; G-NAF refreshes monthly anyway, so
30 days aligns with the upstream data cadence.

## Error budget burn-rate alerts

For SLO-1 and SLO-2 (the user-facing ones), we run **multi-window
burn-rate alerts** per the SRE workbook. The pattern catches both
short, sharp incidents and slow drift.

| Burn rate | Window | Page severity | Budget consumed in window |
|-----------|--------|---------------|---------------------------|
| 14.4× | 5 min | page | 2 % of monthly budget |
| 6× | 30 min | page | 5 % of monthly budget |
| 1× | 1 hour | warn | ~14 % over a month |

The alert rules for these aren't shipped in
`docs/alerts/prometheus-alerts.yaml` yet — they require encoding
the per-endpoint SLO targets as Prometheus expressions (one per
endpoint × window). Add them once you've validated the latency
SLOs against production data; over-tight alerts in week one will
swamp on-call with false positives.

A starter implementation looks like:

```yaml
- alert: GeocoderLatencyBurnFast
  expr: |
    (
      sum by (endpoint) (rate(geocoder_request_duration_seconds_count{endpoint="reverse"}[5m]))
      -
      sum by (endpoint) (rate(geocoder_request_duration_seconds_bucket{endpoint="reverse",le="0.05"}[5m]))
    )
    /
    sum by (endpoint) (rate(geocoder_request_duration_seconds_count{endpoint="reverse"}[5m]))
    > (14.4 * 0.001)   # 14.4× the 99.9% SLO error budget
  for: 2m
  labels: { severity: page }
```

i.e. "the rate of requests slower than the SLO is 14.4× the
error budget rate." Repeat per endpoint × window. Keep the SLO
targets parameterised so this expands automatically; the
[Sloth](https://sloth.dev/) tool generates this from a higher-level
SLO spec if hand-rolling becomes tedious.

## Metric gaps

Two SLIs above reference metrics not yet emitted:

1. `geocoder_request_5xx_total` — per-endpoint 5xx counter. Cheap
   addition, see `tower_http::trace::TraceLayer::on_failure`.
2. `geocoder_index_mtime_seconds` — gauge of newest file mtime in
   the loaded index. Trivial via the existing manifest emission.

Until these ship, use the noted fallbacks (ALB CW metric, deploy
timestamp). Filing them as follow-ups.
