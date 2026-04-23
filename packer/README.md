# AMI builder

Two Packer configs, deliberately split:

- **`geocoder.pkr.hcl`** — the **serving AMI**. Assembles a ready-to-run
  Ubuntu 24.04 image: copies a pre-built `query-server` binary from
  S3, syncs an existing index from S3, installs the systemd unit.
  Fast (~5 min bake), tiny blast radius on failure. This is what you
  use for every normal deploy.

- **`build-worldwide.pkr.hcl`** — a **one-shot index build job** that
  runs on a large-memory Graviton 4 instance with local NVMe
  (`r8gd.16xlarge` by default — 512 GB RAM, 64 vCPU, 3.8 TB NVMe),
  downloads the planet OSM + planet WoF admin data, runs the full
  build pipeline (reverse index → forward index → FST → WoF
  fallback → optional G-NAF / OpenAddresses), and uploads the
  resulting `.bin` files to S3 under a timestamped prefix. Slow
  (~10–12 h on r8gd; ~18–24 h on the r8g fallback) and expensive
  (~$30–80 per build). Run this when the index needs refreshing —
  not on every deploy. The `d` suffix matters: profiling the
  5-country build showed 30–40 % of wall-time in EBS I/O wait
  while osmium's node-location cache churned. Local NVMe
  eliminates that wall; plain `r8g` (no NVMe) falls back to
  EBS-only and runs about 2× slower.

The two configs share no state: the worldwide-build AMI's output
lives in S3, and the serving AMI pulls from that same S3 prefix.
You can rebuild the serving AMI ten times a day against the same
S3 index; you only rebuild the index itself when OSM data has
meaningfully changed.

**Binary compatibility**: the `.bin` files produced by the worldwide
build are fully portable between x86_64 and aarch64 serving hosts
(all are little-endian; our struct layouts are `#[repr(C)]` with no
`usize`/`isize`). You can build on `r8g.16xlarge` and serve from
`c6i.large` or `r7g.medium` without any repackaging. See
[`docs/binary-format.md`](../docs/binary-format.md) for the on-disk
format reference.

## Serving AMI (normal deploys)

Builds an immutable Ubuntu 24.04 LTS AMI pre-loaded with the geocoder
binary and its mmap-backed index files. Single-region build,
copied to N additional regions via `ami_regions`.

## Prerequisites

- **Packer ≥ 1.10** — `brew install packer` / [install docs](https://developer.hashicorp.com/packer/install).
- **Local AWS credentials** with EC2/AMI permissions in the build region
  (Packer uses them to spin up the builder and register the AMI).
- **An IAM instance profile** to attach to the builder EC2 instance. Its
  role needs `s3:GetObject` on the index prefix and the binary, and
  `s3:ListBucket` on the parent bucket so `aws s3 sync` can enumerate.
- **Index and binary already in S3.** The Packer build is an
  *assembler* — it does not build the binary or the index itself.

Example minimal IAM policy for the build instance profile:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": "s3:ListBucket",
      "Resource": "arn:aws:s3:::my-geocoder-artifacts"
    },
    {
      "Effect": "Allow",
      "Action": "s3:GetObject",
      "Resource": [
        "arn:aws:s3:::my-geocoder-artifacts/index/au-latest/*",
        "arn:aws:s3:::my-geocoder-artifacts/bin/query-server-v0.1.0"
      ]
    }
  ]
}
```

## Build

```bash
cd packer
packer init .
cp example.pkrvars.hcl prod.pkrvars.hcl
# edit prod.pkrvars.hcl — set bucket paths, regions, instance profile name
packer build -var-file=prod.pkrvars.hcl geocoder.pkr.hcl
```

Output is a set of AMI IDs — one per region listed in `copy_regions` plus
the build region. Launch instances from them directly or wire into ASGs.

## What lands in the AMI

| Path                              | Contents                                      |
|-----------------------------------|-----------------------------------------------|
| `/usr/local/bin/query-server`     | Geocoder binary (synced from `binary_s3_uri`) |
| `/var/lib/geocoder/index/`        | All mmap index files (synced from `index_s3_uri`) |
| `/etc/systemd/system/geocoder.service` | systemd unit, enabled                    |
| `/etc/default/geocoder`           | Env-file overrides                            |
| `/var/lib/geocoder` (service user home) | Owned by `geocoder` system user         |

The `geocoder` service runs as a dedicated system user with a strict
sandbox (`ProtectSystem=strict`, read-only index dir, no new privs,
no write+exec memory). See `files/geocoder.service`.

## Launching

Any Ubuntu-compatible instance type works. Size memory for the index —
mmap means the working set gets paged in on demand, so small instances
still serve correctly but cold queries are slower. Recommend at least
`c6i.large` (2 GB) for AU, more for worldwide.

Minimal launch:

```
Instance type: c6i.large
Security group: inbound 3000/tcp from ALB / your client
IAM instance profile: (none needed for serving — index is baked in)
```

To override the bind address post-deploy without rebuilding the AMI:

```bash
sudo sed -i 's|^BIND_ADDR=.*|BIND_ADDR=0.0.0.0:8080|' /etc/default/geocoder
sudo systemctl restart geocoder
```

## Worldwide index build job

```bash
cd packer
cp worldwide.pkrvars.example.hcl worldwide.pkrvars.hcl
# edit worldwide.pkrvars.hcl — bucket prefix, IAM profile, region
make -C .. ami-worldwide-build PKRVARS=packer/worldwide.pkrvars.hcl
```

What happens:

1. Packer launches an `r8g.16xlarge` (or whatever `instance_type`
   you set) with a 1 TB gp3 root EBS.
2. Stage-by-stage provisioners install toolchain, clone the repo,
   build the C++ + Rust binaries, fetch the planet PBF (~75 GB) +
   planet WoF admin (~8.6 GB bz2), and run the full build
   pipeline.
3. The resulting `data/index-worldwide/` directory is uploaded to
   S3 under `${output_s3_prefix}${timestamp}/` plus a
   `${output_s3_prefix}latest/` alias.
4. SHA-256 checksums are emitted as `SHA256SUMS` in the uploaded
   tree so serving-side sync can verify integrity.
5. The AMI is snapshotted for audit and the build instance
   terminates.

The IAM instance profile needs `s3:PutObject` (and `s3:DeleteObject`
if you want the `latest/` alias to replace old files cleanly) on
`${output_s3_prefix}*`, plus `s3:ListBucket` on the parent bucket.

After the worldwide build lands, run the serving AMI build pointed
at the fresh S3 prefix (typical flow: weekly or monthly worldwide
rebuild → dozens of serving AMIs per week from the same S3 index).

## Updating the index

Two patterns work, each matching different operational preferences:

1. **New AMI per index.** The canonical cloud-native flow: rebuild the
   AMI when the index changes, do a blue/green rollout via ASG. This
   repo's Packer config is set up for that.
2. **In-place sync via hot-reload.** Leave instances long-running, sync
   a fresh index to `/var/lib/geocoder/index`, `touch` the reload
   marker. The service atomically swaps mmaps without restart (see
   `GEOCODER_RELOAD_MARKER` in `files/geocoder.env`). Simpler to
   operate for small fleets; loses the "immutable infrastructure"
   property.

Pick one and stick with it — mixing causes drift.

## Troubleshooting

- `packer build` hangs during S3 sync → the instance profile lacks
  `s3:ListBucket` or the referenced prefix is empty.
- AMI registered but instances boot without the service → check
  `journalctl -u geocoder` on a launched instance; typical cause is a
  permission mismatch on `/var/lib/geocoder/index` (must be readable
  by `geocoder` user).
- Instance runs but index dir is empty → `aws s3 sync --delete` was
  invoked against an empty prefix. Confirm `index_s3_uri` points at
  the actual files, not the bucket root.
