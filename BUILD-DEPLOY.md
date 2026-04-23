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
  - `cmake`, `g++`, `libosmium2-dev`, `libs2-dev`, `libprotozero-dev`
    (Dockerfile-equivalent, or use Docker)
  - `osmium-tool`, `pyosmium-get-changes` (for index updates)
- **S3 bucket** for artifacts (index files + binary). No special config needed.

## Step 1 — build the index

Run on a beefy machine (16+ GB RAM for AU, 64+ GB for worldwide).
Reuses the existing pipeline scripts:

```bash
# a) download OSM PBF
./scripts/download-region.sh australia ./data/pbf

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

### Logs

`systemd-journald` captures `query-server`'s stderr output. Forward to
CloudWatch Logs via the
[CloudWatch agent](https://docs.aws.amazon.com/AmazonCloudWatch/latest/monitoring/install-CloudWatch-Agent-on-EC2-Instance.html) —
install it in the AMI (one more `apt-get install` line in the Packer
provisioner) and point its config at `journal`.

### Metrics

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
