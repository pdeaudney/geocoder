# Alerts

Prometheus alert rules for the query server, plus guidance for the
headline panels operators should build into a Grafana / Honeycomb
dashboard.

## Wiring the rules

`prometheus-alerts.yaml` is a stock Prometheus rules file. Drop it
under whatever path your Prometheus config's `rule_files:` glob
covers — typically `/etc/prometheus/rules/`.

```yaml
# prometheus.yml
rule_files:
  - /etc/prometheus/rules/*.yaml
```

Then reload Prometheus (`SIGHUP` or `POST /-/reload`) and confirm the
rules show in `Status → Rules`.

The rules reference `aws_alb_healthy_host_count` for the
healthy-host alert — if you're not on AWS, swap the metric name in
the `GeocoderInstancesUnhealthy` rule for whatever your platform
exposes (`up{job="geocoder"} == 0`, `kube_pod_status_ready`, etc.).

## Severity convention

- `severity: page` → wakes someone up. Service-affecting. Routes to
  PagerDuty / Opsgenie / equivalent.
- `severity: warn` → file a ticket, handle in business hours.

The PagerDuty / Slack routing isn't included here — Alertmanager
configuration is operator-specific. Match on `severity` and route
accordingly.

## Recording rules

Three recording rules pre-aggregate the most-queried expressions:

- `geocoder:request_duration_seconds:p99_5m` — per-endpoint p99 over
  5 minutes.
- `geocoder:requests:rate_5m` — per-endpoint RPS.
- `geocoder:shadow_match_rate_5m` — per-endpoint accuracy vs Google.

Cheaper to query than the raw histograms. Use these in dashboards.

## Headline panels for Grafana

A full dashboard JSON would be hundreds of lines and quickly drift
from the underlying queries. Instead, here are the six panels every
geocoder dashboard should carry. Build them in Grafana using these
queries:

### 1. Overall RPS

```promql
sum(rate(geocoder_requests_total[1m]))
```

Time series. Helps spot traffic drops + capacity-plan input.

### 2. RPS per country

```promql
sum by (country) (rate(geocoder_requests_total{endpoint="search"}[1m]))
```

Time series with `country` as the legend. Shows per-region traffic
mix; alerts when a major country drops to zero.

### 3. p50 / p99 latency per endpoint

```promql
histogram_quantile(0.5,  sum by (le, endpoint) (rate(geocoder_request_duration_seconds_bucket[5m])))
histogram_quantile(0.99, sum by (le, endpoint) (rate(geocoder_request_duration_seconds_bucket[5m])))
```

Time series. Two queries on one panel; `endpoint` as legend.

### 4. Shadow match rate vs Google

```promql
geocoder:shadow_match_rate_5m
```

Stat panel + time series. Headline accuracy number.

### 5. Shadow distance histogram (search only)

```promql
histogram_quantile(0.95, sum by (le) (rate(geocoder_shadow_distance_meters_bucket{endpoint="search"}[5m])))
```

Single value or heatmap. p95 distance from Google's top-1.

### 6. Shadow outcome breakdown

```promql
sum by (outcome) (rate(geocoder_shadow_outcomes_total[5m]))
```

Stacked time series. Should be dominated by `match`; spikes in
`google_error`, `rate_limited`, `auth_disabled` are diagnostics.

## Testing alerts before they go live

Use `promtool test rules` against `prometheus-alerts.yaml` with a
synthetic input series to verify each alert fires when it should:

```bash
promtool check rules docs/alerts/prometheus-alerts.yaml
```

A canned `.test.yaml` companion (input metric values + expected
firings) is the standard pattern; we don't ship one because synthetic
data depends on your Prometheus version's recording-rule semantics.
The runbook at `RUNBOOK.md` documents the response procedure for
each alert.
