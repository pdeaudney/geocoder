# Runbook

Operational procedures for the geocoder query server. Keep
this short — alert-driven rather than encyclopedic. If you find
yourself adding background to an entry, the background belongs in
[`ARCHITECTURE.md`](ARCHITECTURE.md) or [`BUILD-DEPLOY.md`](BUILD-DEPLOY.md);
this file is "what do I press at 3 am."

Conventions:
- AWS terminology used throughout (ASG, ALB, EBS, AMI). For k8s
  deployments, see [`docs/kubernetes-deployment.md`](docs/kubernetes-deployment.md);
  the substantive procedures translate directly.
- All anchors used in `docs/alerts/prometheus-alerts.yaml` annotations are
  in this file. If you rename a section, update the YAML in the same PR.

## Service basics

- **Process**: single Rust binary `query-server`, default port 3000
  (REST) + 3001 (gRPC).
- **Index location on instance**: `/var/lib/geocoder/index/`.
- **Logs**: stdout in JSON; captured by `journald`, shipped to
  CloudWatch Logs / Loki by the agent.
- **Service unit (systemd, on AMI)**: `geocoder.service`.

## Common operations

### Restart the service
```bash
sudo systemctl restart geocoder
```
`/healthz/ready` returns 503 during the boot — the LB stops routing
traffic to this instance until the index has finished mmapping.
Typical cold-boot ready time on `c6i.large` with the AU index is
under 10 s.

### Reload the index without restart
The query server watches a marker file (default
`/var/lib/geocoder/index/.reload`) and atomically swaps the in-memory
index when its mtime changes:
```bash
sudo touch /var/lib/geocoder/index/.reload
```
Background reloader picks it up within `GEOCODER_RELOAD_INTERVAL_SEC`
seconds (default 5). Old `Arc<Index>` continues serving in-flight
requests; new ones go to the new index. No restart needed.

### Drain a host before destruction
The simplest drain is to stop replying ready while keeping live:
```bash
# On the instance:
sudo systemctl stop geocoder   # /healthz/* both fail → ALB drains
```
For graceful drain (let in-flight requests complete), prefer
`systemctl reload` semantics (TODO: not yet implemented; for now use
ALB target deregistration which respects the deregistration delay).

### Verify shadow-validation is healthy
```bash
curl -s http://<instance>:3000/metrics | grep '^geocoder_shadow_'
```
Look for:
- `geocoder_shadow_outcomes_total{outcome="match"}` — should dominate.
- `geocoder_shadow_outcomes_total{outcome="auth_disabled"}` — must be 0
  (or static — non-zero rate is the `GeocoderShadowAuthDisabled` alert).
- `geocoder_shadow_queue_full_total` — should be flat (rate ≈ 0).

## Index rollback

You've deployed an index that's serving wrong data. The new index is
at `/var/lib/geocoder/index/`; the previous index is in
`/var/lib/geocoder/index-prev/` (kept by `update-index.sh` if used)
or in S3 under the previous version prefix.

### From a kept previous version
```bash
# On each instance:
cd /var/lib/geocoder
sudo mv index index.bad-$(date -u +%Y%m%dT%H%M%S)
sudo mv index-prev index
sudo touch index/.reload
```
Server picks up the swap on the next reload cycle. Verify with
`scripts/smoke-test.sh https://<lb>:3000 au` from a control host.

### From S3 (no local previous version)
```bash
sudo aws s3 sync s3://$BUCKET/index/au-$PREV_VERSION/ /var/lib/geocoder/index-rollback/ --delete
sudo systemctl stop geocoder
sudo mv /var/lib/geocoder/index /var/lib/geocoder/index.bad
sudo mv /var/lib/geocoder/index-rollback /var/lib/geocoder/index
sudo systemctl start geocoder
```
`/healthz/ready` is 503 until index loads; the LB removes this
instance from rotation until then.

### Validate the rollback
Run the regression suite against the rolled-back instance from a
control host:
```bash
./scripts/run-regression.sh http://<rollback-instance>:3000
```
A pass means the rollback restored a known-good state.

## Pre-deploy index validation

**Always** run `regression-runner` against a new index before
rotating it into production:
```bash
./target/release/query-server data/index-candidate 0.0.0.0:13099 &
sleep 5
./scripts/run-regression.sh http://localhost:13099
```
A failing regression means the index has accuracy regressions; do
not roll out.

---

## Alert response procedures

Each section below maps to an alert in
[`docs/alerts/prometheus-alerts.yaml`](docs/alerts/prometheus-alerts.yaml).

### high-p99-latency

**Trigger:** `geocoder:request_duration_seconds:p99_5m > 0.1` for 5 min.

**First three minutes:**
1. Check the affected endpoint label — is it just one (`/search`) or
   all (`/reverse`, `/search`, `/autocomplete`)?
2. Check `RequestCountPerTarget` in CloudWatch — has traffic spiked?
3. Check the OS page-cache pressure on each healthy instance:
   ```bash
   ssh <instance>
   free -m   # used should leave ≥ 1G free for kernel page cache
   ```

**Common causes / fixes:**
- mmap page-in storm after a deploy → wait it out (seconds to a
  minute on warm instances), then add a warm-up curl loop to the
  AMI's first-boot script.
- Kernel page cache evicted by noisy neighbour → migrate to a
  dedicated host, or increase instance class.
- ASG undersized → scale out via launch-template or temporarily
  set min size higher.

### traffic-disappearance

**Trigger:** RPS drops below 50% of last hour's average for 5 min.

**First three minutes:**
1. ALB → Target Group → Healthy host count. If < 2, see
   [instances-unhealthy](#instances-unhealthy) instead.
2. If hosts healthy → upstream issue. Check the gateway / LB rules.
3. Recent deploys to anything in front of the geocoder?

### shadow-export-errors

**Trigger:** `rate(google_error|timeout) > 0.1/s` for 5 min.

User-facing service is unaffected (shadow is fire-and-forget). To
investigate:
1. Confirm the `GOOGLE_GEOCODING_API_KEY` env var is still set on
   instances.
2. Test outbound HTTPS to `maps.googleapis.com` from one instance.
3. Check
   [Google Maps Platform status page](https://status.cloud.google.com/maps-platform/products/geocoding-api).
4. If long-running and your accuracy dashboards depend on shadow
   data, set `GOOGLE_GEOCODING_ENABLED=false` on the AMI's user data
   and roll instances. Re-enable once the upstream issue clears.

### shadow-auth-disabled

**Trigger:** `rate(outcome="auth_disabled") > 0` for 1 min.

**Sticky** — once `REQUEST_DENIED` triggers, the worker stays
disabled until process restart. Probable causes:
- Key revoked or rotated without updating the instance env.
- Billing account problem on the Google project.

**Fix:**
1. Verify the key works:
   ```bash
   curl -s "https://maps.googleapis.com/maps/api/geocode/json?latlng=-33.86,151.21&key=$KEY" | jq .status
   ```
   Expect `OK`. If `REQUEST_DENIED`, fix the key in Google Cloud
   Console first.
2. Update the secret in your secret manager.
3. Roll instances so they pick up the new key + restart the disabled
   shadow workers.

### shadow-queue-full

**Trigger:** `rate(geocoder_shadow_queue_full_total) > 0.01/s` for 10 min.

Shadow accuracy data has gaps. Service unaffected. Diagnosis:
1. `GOOGLE_GEOCODING_SAMPLE_RATE` is too high for current traffic.
2. Worker is back-pressured by Google rate limits (check
   `outcome="rate_limited"` rate too).

**Fix:**
- Lower `GOOGLE_GEOCODING_SAMPLE_RATE` (e.g. 0.001 → 0.0005).
- Or raise `GOOGLE_GEOCODING_QUEUE_CAPACITY` (default 256).
- Or raise `GOOGLE_GEOCODING_INFLIGHT_CAP` (default 4) if you have
  budget.

### accuracy-regression

**Trigger:** `geocoder:shadow_match_rate_5m < 0.85` for 30 min.

Compare the current rate to the last week's baseline first — if
Google changed something on their side (their data does drift), our
match rate moves. To distinguish:
1. Check the per-axis breakdown:
   ```promql
   sum by (axis) (rate(geocoder_shadow_match_total{result="match"}[1h]))
     / sum by (axis) (rate(geocoder_shadow_match_total{result=~"match|mismatch"}[1h]))
   ```
   `country` should be ~100%; `state`, `city`, `road` lower. If
   `country` itself dropped, suspect Google.
2. Pull a sampled mismatch log:
   ```bash
   journalctl -u geocoder --since "1 hour ago" \
     | jq -r 'select(.target=="query_server::shadow")
         | select(.fields.message=="shadow mismatch")
         | "\(.fields.our_formatted) vs \(.fields.google_formatted)"' \
     | head -10
   ```
   Eyeball — are the mismatches our fault?

If the regression is on our side, file a bug + plan an index rebuild.

### instances-unhealthy

**Trigger:** ALB healthy host count < 2 for 10 min.

1. Check the ALB target group → target health → reason. Common:
   - "Health checks failed" → instance is up but `/healthz/ready` is
     503. mmap page-in too slow, or index load failed. SSH in:
     ```bash
     journalctl -u geocoder --since "5 minutes ago"
     ```
   - "Target deregistered" → recent deploy in progress.
2. If instances are in ASG → check the ASG's "Activity history" tab
   for failed launches. AMI ID, launch template, IAM profile are the
   usual culprits.
3. If you're below the desired count and need capacity NOW, set the
   ASG min to the desired count manually so it fills.
