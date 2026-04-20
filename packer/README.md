# AMI builder

Build an immutable Ubuntu 24.04 LTS AMI pre-loaded with the geocoder
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
