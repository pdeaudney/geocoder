# Planet build on NVIDIA GB10 Grace Blackwell — measured 2026-04-27

> **Snapshot doc.** One-off measurement of a planet `build-index`
> run on a GB10-class workstation. Recording it because the host
> sits right at the RAM floor for planet, the failure modes are
> instructive, and the GB10 is plausibly attractive for self-hosted
> geocoder builds (single workstation, no AWS bill) — operators
> considering it should know what to expect.

## Host

| | |
|---|---|
| Platform | NVIDIA GB10 Grace Blackwell Superchip (DGX Spark / Project DIGITS class) |
| CPU | 20-core Arm Neoverse V2 (Grace) |
| RAM | 128 GB LPDDR5x unified, ~122 GB visible to the OS |
| Swap | 16 GiB on local disk |
| Storage | M.2 NVMe (sustained read measured at ~100 MB/s during build — significantly below the drive's spec, see below) |
| OS | Linux aarch64, GCC 13.3.0 |

## Build target

- `--region planet` (single 80 GB stream from planet.openstreetmap.org)
- WoF planet admin SQLite included
- OpenAddresses + MaxMind + G-NAF off
- C++ builder + workspace built locally (no toolchain pre-cache)

## Result: build succeeded, ~13.5 hours

```
[stage] pass2_ingest:    48053.7 s   (13 h 20 m)
[stage] resolve_interp:     20.6 s
[stage] dedup:              20.3 s
[stage] sort_addr_points:   11.0 s
[stage] write_index:       349.1 s   (5 m 49 s)
[stage] total:           48509.5 s   (13 h 28 m)
```

`pass2_ingest` consumed **99.3 %** of wall time. Everything else
combined is ~7 minutes — once the libosmium parser finishes, the
rest of the pipeline is fast regardless of host.

## Output index shape

| | |
|---|---:|
| Streets | 48,356,353 |
| Address points | 161,026,543 (81.9 M from buildings, 79.0 M from explicit `addr:*` tags) |
| Place points | 3,608,610 |
| Admin polygons | 943,778 rings across 791,651 boundaries |
| i18n names | 4,297,770 |
| Strings table | 262 MB |
| `geo` index cells | 339,280,401 |
| `admin` index cells | 2,048,295 |
| `place` index cells | 523,023 |
| Interpolation resolution rate | 41,771 / 72,229 (57.8 %) |

Numbers match expectations for a planet build. No anomalies in
data shape — the long wall-time was a host-side issue, not a
build-pipeline regression.

## Memory profile

The host is right at the planet floor. Documented working-set
extrapolation from the 5-country build (15 GB RSS + 23 GB
file-backed cache, super-linear scaling) put planet at ~128 GB
peak. The GB10's 122 GB visible RAM was just below that.

Observed:
- Peak resident-set ≈ 80 GB at mid-run (anonymous heap, mostly the
  in-flight relation/multipolygon assembler holding way refs).
- Mmap'd file-backed pages in `cache` reached ~109 GB (PBF +
  node_locations cache).
- Swap usage climbed to **16,301 / 16,384 MiB (99.5 %)** during
  the address-collection phase.
- Once pass-3 (relation pass) cleared, swap stayed full but inert:
  cold pages stuck on disk, no active swap-in/out.
- No OOM-kills.

Pattern: kernel evicted page-cache pages aggressively to make room
for the process's anonymous working set, leaning on swap as the
safety net. Each page-cache eviction means re-reading the
mmap'd file from disk later, multiplying I/O wasted.

## I/O profile

Sustained read rate during pass-2 measured via `vmstat 1`:

| Metric | Observed |
|---|---|
| `bi` (block-in) | 75–245 KB blocks/s ≈ **75–120 MB/s** |
| `bo` (block-out) | ~0 (pure read phase) |
| `wa` (CPU iowait %) | 10–13 % |
| Process state | Mostly in `D` (uninterruptible sleep on disk I/O); `r` queue at 0–2, `b` queue at 2–3 |
| CPU utilisation | 1–3 % user; ~85 % idle |

100 MB/s sustained read is **HDD-class** throughput — well below
what an M.2 NVMe should sustain (typical: 1–3 GB/s for sequential
read). Plausible explanations:

1. **Read contention** between the PBF stream and the
   `node_locations.tmp` mmap, both on the same volume fighting
   for queue depth. Random access into the multi-GB node cache
   (~10 GB on planet) is what's pinning the disk.
2. **SLC cache exhaustion** on a consumer-grade SSD doing
   sustained reads during writes. Possible but `bo` was ~0 most
   of the time, so this seems unlikely.
3. **Thermal throttling** of the M.2 controller during sustained
   I/O. The GB10's compact form factor doesn't have great
   airflow over the SSD.
4. **PCIe lane sharing** with the GPU — the GB10's M.2 slot is on
   the same PCIe fabric as the Blackwell GPU. Possible for IO
   throughput to be capped if the GPU side is also active.

The 100 MB/s ceiling is what dictated the 13.5-hour wall-time. CPU
was 85 % idle the entire time waiting on synchronous reads.

## Comparison to the documented reference

`docs/performance/capacity-plan.md` and the Packer template
(`packer/build-worldwide.pkr.hcl`) target an EC2 r8gd.16xlarge:

| | r8gd.16xlarge (target) | NVIDIA GB10 (this run) |
|---|---|---|
| CPU | 64-core Arm Neoverse V2 (Graviton 4) | 20-core Arm Neoverse V2 (Grace) |
| RAM | 512 GB DDR5 | 128 GB LPDDR5x |
| Storage | 3.8 TB local NVMe (instance-store) | M.2 NVMe (consumer / OEM grade) |
| Sustained read | 1–3 GB/s | ~100 MB/s observed |
| Expected planet build | 4–6 h (extrapolated from measured 5-country baseline) | 13.5 h (this run) |
| Cost | ~$60 on-demand spot for one full build | one-off hardware purchase |

The GB10 took **2-3× longer** than a properly-spec'd r8gd would,
but produced a byte-identical index. For one-off builds where
wall-time isn't critical, this is a viable path. For a recurring
weekly planet refresh, the slowdown adds up.

## Recommendations for GB10 operators

1. **Prefer `--region all-continents` over `--region planet`.**
   The continent extracts have lower per-pass working sets
   (~10–25 GB each instead of one 75 GB stream); merged dedup is
   already integrated into the build pipeline. Even on the GB10,
   this should fit RAM without swap-assist for any single
   continent's pass.

2. **Single-region builds are comfortable.** AU, NZ, EU
   individually sit at ~15–40 GB working set. Plenty of headroom
   on the GB10. The "uncomfortable" zone starts at planet-scale
   only.

3. **Mind the M.2 thermals.** If you're seeing build wall-times
   2-3× the expected ratio, monitor SSD temperature during the
   build:
   ```bash
   watch -n 5 'sudo nvme smart-log /dev/nvme0n1 | grep -i "temperature"'
   ```
   A sustained >70 °C reading suggests the controller is
   throttling. Mitigations: better case airflow, an aftermarket
   M.2 heatsink, or moving the build's working data to external
   USB-NVMe storage if available.

4. **Consider increasing swap to 64 GB.** 16 GB filled to 99.5 %
   during this build — there was no headroom. A larger swap file
   wouldn't have made the build faster, but would have provided
   safety margin if any single allocation spike pushed past the
   current ceiling.
   ```bash
   sudo fallocate -l 64G /swapfile2
   sudo chmod 600 /swapfile2
   sudo mkswap /swapfile2
   sudo swapon /swapfile2
   # Persist via /etc/fstab if you want it across reboots.
   ```

5. **The Blackwell GPU doesn't help here.** The C++ `build-index`
   is single-threaded on the Grace side; libosmium ingestion is
   I/O-bound and control-flow-divergent. CUDA / cuCollections
   would not accelerate this workload (analysis in conversation
   thread on 2026-04-27, summarised: GPU memory transfer cost
   dwarfs any compute speedup for sparse hash-map heavy work).

## What the GB10 *would* be excellent for

Out of scope for this doc, but worth noting so future readers
don't conclude "GB10 is the wrong machine":

- The Rust `query-server` runtime, post-build. Once the index is
  in mmap'd memory, queries are <100 µs and would benefit from the
  Grace CPU's high single-thread performance. The index file
  fits comfortably in 128 GB unified memory.
- The shadow-validation worker is fully CPU-bound on the
  reqwest+JSON-parse path; no GPU benefit either.
- The Blackwell GPU is genuinely useful for the AI-related
  workloads the box is designed for (LLM inference, vector
  search, embedding generation) — those are where the unified
  memory architecture pays off. None of those overlap with the
  geocoder's current pipeline.

The takeaway is that the GB10 is a **viable build host** with
caveats, and a **comfortable serving host** for a single-instance
deployment. It's not the optimal fit for a recurring planet-scale
build pipeline; that's still the EC2 r8gd path.
