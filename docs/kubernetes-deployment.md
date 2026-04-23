# Kubernetes deployment

This document describes the Kubernetes-native alternative to the
AMI-based flow in [`BUILD-DEPLOY.md`](../BUILD-DEPLOY.md). Same
binary, same index format, different orchestration. Teams already
running EKS / GKE / vanilla k8s on AWS will usually find this
easier to slot in than maintaining a parallel AMI pipeline.

Cross-references:
- [`docs/binary-format.md`](binary-format.md) — why mmap workloads
  need local block storage and can't tolerate network filesystems
  without penalty.
- [`docs/worldwide-build.md`](worldwide-build.md) — where the
  index artifact comes from upstream of this doc.
- [`packer/build-worldwide.pkr.hcl`](../packer/build-worldwide.pkr.hcl)
  — the one-shot EC2 build job that produces the S3 artifact this
  pipeline consumes.

## TL;DR

**Recommended pattern**: per-pod PVC restored from an EBS
`VolumeSnapshot` that represents a frozen copy of the built index.

```
          ┌──────────────────────┐
          │  worldwide-build     │
          │  (Packer, EC2 r8g)   │ runs weekly; writes to S3
          │  18–24 h, ~$50       │
          └──────────┬───────────┘
                     │ aws s3 sync
                     ▼
          ┌──────────────────────┐
          │  S3: indexes/latest  │ source of truth
          └──────────┬───────────┘
                     │
                     ▼
          ┌──────────────────────┐
          │  snapshot-refresh    │ Job: populate PVC from S3, snapshot,
          │  (in-cluster)        │ promote to `geocoder-index-current`.
          │  ~20 min             │ Runs after each upstream S3 refresh.
          └──────────┬───────────┘
                     │ VolumeSnapshot
                     ▼
          ┌──────────────────────┐
          │  StatefulSet:        │
          │   geocoder           │ each pod's PVC provisioned from
          │   replicas: N        │ the current snapshot, local-EBS
          │                      │ performance, zero NFS penalty.
          └──────────────────────┘
                     │
                     ▼
          ┌──────────────────────┐
          │ Service: geocoder    │ ClusterIP; an Ingress/Gateway
          │ :3000                │ handles TLS and external routing.
          └──────────────────────┘
```

Each pod gets its own ~200 GB gp3 EBS volume materialised from the
same snapshot. Storage cost scales with pod count (~$16/pod/month
for 200 GB gp3 at $0.08/GB). In exchange: native mmap performance,
zero distributed-filesystem weirdness, AZ-correct attachment,
independent page caches per pod.

## Why not `ReadOnlyMany` over a shared EBS volume?

| Option | Verdict | Why |
|---|---|---|
| gp3 `ReadOnlyMany` | ❌ impossible | EBS gp3/gp2 are single-attach at the block-device level. CSI can't expose them to multiple nodes. |
| `io2` multi-attach + RO | ❌ not viable | Max 16 instances, single AZ, "not supported for common filesystems". ext4/xfs assume single-writer and silently corrupt on concurrent mount. |
| EFS (NFS) | ⚠️ works but slow | Native multi-mount. But mmap over NFS: close-to-open consistency degrades page cache, cold reads serialise through the NFS client. Measured penalty for comparable workloads: ~3–10× p99 latency on cold queries. |
| Mountpoint-S3 (FUSE) | ❌ wrong tool | Streaming-read optimized, not mmap. FUSE page-cache semantics are driver-specific; cold path is slow. |
| **Per-pod PVC from snapshot** | ✅ **recommended** | Native block device, full kernel page cache, AZ-isolated attachment, standard k8s StatefulSet pattern. Cost is just N × index-size × $0.08/GB/month. |

Our server memory-maps the `.bin` files and
counts on the kernel page cache to serve repeated reads. That
caching is only free on local block storage. Any network-filesystem
path adds serialisation, close-to-open consistency flushes, and
inconsistent page-cache semantics. Those are all invisible on a benchmark,
painful in production p99.

## Prerequisites

- Kubernetes ≥ 1.26 with the VolumeSnapshot CRDs
  (`volumesnapshotclasses.snapshot.storage.k8s.io` etc.). Most
  managed control planes (EKS, GKE) have these enabled.
- AWS EBS CSI driver installed (`aws-ebs-csi-driver` helm chart or
  the EKS add-on). Test: `kubectl get sc gp3` should show a
  StorageClass backed by `ebs.csi.aws.com`.
- Pod identity for the build-refresh Job to read from S3. Use
  EKS Pod Identity (preferred) or IRSA.

## Artifacts

Three things this pipeline needs:

1. **Container image** running `query-server` (binary-only;
   listens on 3000, reads the index from `/var/lib/geocoder/index`,
   `/healthz` for probes).
2. **Source of truth in S3** — the same bucket path the Packer
   worldwide-build job writes to.
3. **Snapshot-refresh Job** — seed a PVC from S3 once, snapshot
   it, bump the canonical name that the StatefulSet reads.

## StorageClass (AWS EBS gp3)

```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: geocoder-index-sc
provisioner: ebs.csi.aws.com
parameters:
  type: gp3
  # gp3 baseline: 3000 IOPS / 125 MB/s. The index workload is
  # read-heavy but page-cache-friendly, so baseline is usually
  # enough. Bump these if your index regularly exceeds 100 GB
  # working set per pod (rare) or you see cold-start IOPS spikes.
  iops: "3000"
  throughput: "125"
  encrypted: "true"
  # gp3 lets us specify fs — defaults to ext4 which is fine.
  # Index files are immutable at runtime, so a read-only bind
  # inside the pod further guards against accidental writes.
reclaimPolicy: Delete
volumeBindingMode: WaitForFirstConsumer
allowVolumeExpansion: true
```

`WaitForFirstConsumer` is important: it defers PVC provisioning
until a pod is scheduled so the volume lands in the pod's AZ.
Without it, you can get an AZ mismatch that prevents attachment.

## VolumeSnapshotClass

```yaml
apiVersion: snapshot.storage.k8s.io/v1
kind: VolumeSnapshotClass
metadata:
  name: geocoder-snap
driver: ebs.csi.aws.com
deletionPolicy: Retain   # don't delete snapshots when the
                         # VolumeSnapshot resource is deleted;
                         # we want to keep old snapshots for
                         # rollback.
parameters:
  # Explicit CSI parameters — most clusters leave this empty and
  # inherit EBS defaults.
  tagSpecification_1: "Name=geocoder-index"
  tagSpecification_2: "App=geocoder"
```

## Snapshot-refresh Job

The trickiest piece. A Job pattern that:

1. Runs once per upstream S3 refresh (weekly/monthly cadence).
2. Provisions a fresh "builder" PVC with no data.
3. Mounts the PVC, runs `aws s3 sync` from the upstream S3 prefix.
4. Creates a `VolumeSnapshot` from the populated PVC.
5. Updates a config map / label that the StatefulSet reads, so
   rolling restart picks up the fresh snapshot for new PVCs.
6. Deletes the builder PVC (but keeps the snapshot).

A reasonable implementation (one manifest, no Argo/Flux
dependencies — plain Job + BusyBox-style image with `aws` CLI):

```yaml
# Step 1: builder PVC — large enough to hold the full index.
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: geocoder-index-builder
spec:
  accessModes: ["ReadWriteOnce"]
  storageClassName: geocoder-index-sc
  resources:
    requests:
      storage: 250Gi   # headroom for future growth
---
# Step 2: Job that populates it from S3 and snapshots.
apiVersion: batch/v1
kind: Job
metadata:
  name: geocoder-index-refresh
spec:
  backoffLimit: 1
  template:
    spec:
      serviceAccountName: geocoder-index-refresh  # IRSA / Pod Identity
      restartPolicy: OnFailure
      containers:
      - name: refresh
        image: amazon/aws-cli:2.17.0
        command: ["/bin/sh", "-c"]
        args:
        - |
          set -eux
          # S3 prefix where Packer's worldwide-build job uploads.
          SRC="$${S3_SOURCE_PREFIX}"
          DEST=/var/lib/geocoder/index
          mkdir -p "$${DEST}"
          aws s3 sync --no-progress --delete "$${SRC}" "$${DEST}/"
          # Verify checksum manifest emitted by the build job.
          (cd "$${DEST}" && sha256sum -c SHA256SUMS)
          echo "index size:"
          du -sh "$${DEST}"
        env:
        - name: S3_SOURCE_PREFIX
          value: "s3://my-geocoder-artifacts/index/worldwide/latest/"
        volumeMounts:
        - name: index
          mountPath: /var/lib/geocoder/index
      volumes:
      - name: index
        persistentVolumeClaim:
          claimName: geocoder-index-builder
---
# Step 3: after the Job completes, snapshot the builder PVC.
# (Typically triggered by a subsequent Job or by Argo Workflows;
# shown inline here for documentation.)
apiVersion: snapshot.storage.k8s.io/v1
kind: VolumeSnapshot
metadata:
  name: geocoder-index-2026-04-23
  labels:
    app: geocoder
    rev: "2026-04-23"   # increment this for each refresh
spec:
  volumeSnapshotClassName: geocoder-snap
  source:
    persistentVolumeClaimName: geocoder-index-builder
```

After the snapshot is `readyToUse: true`, serving StatefulSet pods
provision their PVCs from it.

**Alternative pre-populated snapshot**: AWS EBS supports
"data source" on PVCs — you can also directly create a new
EBS volume from an S3 URI via the `FastSnapshotRestore` feature,
skipping the in-cluster sync entirely. Cleaner but costs FSR
pricing (~$0.75/TB/hr after the first hour). Usually cheaper to
run the in-cluster sync job.

## StatefulSet

```yaml
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: geocoder
  labels:
    app: geocoder
spec:
  serviceName: geocoder
  replicas: 4
  selector:
    matchLabels:
      app: geocoder
  template:
    metadata:
      labels:
        app: geocoder
    spec:
      # Spread pods across AZs. Each pod's PVC will be provisioned
      # from the snapshot in its AZ (EBS snapshots are cross-AZ
      # automatically via the CSI driver).
      topologySpreadConstraints:
      - maxSkew: 1
        topologyKey: topology.kubernetes.io/zone
        whenUnsatisfiable: ScheduleAnyway
        labelSelector:
          matchLabels:
            app: geocoder
      containers:
      - name: query-server
        image: ghcr.io/pdeaudney/geocoder:0.1.0
        args: ["/var/lib/geocoder/index", "0.0.0.0:3000"]
        ports:
        - containerPort: 3000
          name: http
        readinessProbe:
          httpGet:
            path: /healthz
            port: http
          initialDelaySeconds: 10   # mmap page-in warms up fast;
                                    # bump to ~30s for worldwide
                                    # indexes if you see flaky
                                    # readiness on cold start.
          periodSeconds: 5
          failureThreshold: 3
        livenessProbe:
          httpGet:
            path: /healthz
            port: http
          periodSeconds: 30
          failureThreshold: 3
        resources:
          requests:
            cpu: "1"
            memory: "4Gi"
          limits:
            cpu: "4"
            memory: "16Gi"
        volumeMounts:
        - name: index
          mountPath: /var/lib/geocoder/index
          readOnly: true
        # Locked-down pod security context; the server only needs
        # read-only access to the index and can't write anywhere.
        securityContext:
          allowPrivilegeEscalation: false
          readOnlyRootFilesystem: true
          capabilities:
            drop: ["ALL"]
          runAsNonRoot: true
          runAsUser: 10001
          runAsGroup: 10001
          seccompProfile:
            type: RuntimeDefault
  # PVC per pod, hydrated from the current snapshot.
  volumeClaimTemplates:
  - metadata:
      name: index
    spec:
      accessModes: ["ReadWriteOnce"]
      storageClassName: geocoder-index-sc
      resources:
        requests:
          storage: 250Gi
      dataSource:
        name: geocoder-index-current
        kind: VolumeSnapshot
        apiGroup: snapshot.storage.k8s.io
```

Two subtleties:

1. **`geocoder-index-current`** is a convention, not a built-in.
   You need a process that, after each refresh, points this
   snapshot name at the latest underlying snapshot. Simplest:
   a post-refresh step that creates a `VolumeSnapshot` named
   `geocoder-index-current` with `dataSourceRef` pointing at the
   freshest timestamped snapshot. Or: use a label selector and a
   VolumeSnapshotContent alias pattern (more complex, less common).
2. **`readOnly: true` at the volume mount level** is a defence-
   in-depth measure. The server writes nothing at runtime; making
   the mount read-only catches any accidental write attempt early
   and documents intent.

## Service + Ingress

```yaml
apiVersion: v1
kind: Service
metadata:
  name: geocoder
  labels:
    app: geocoder
spec:
  type: ClusterIP
  selector:
    app: geocoder
  ports:
  - name: http
    port: 3000
    targetPort: 3000
---
# Example AWS ALB Ingress (for EKS with AWS Load Balancer Controller)
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: geocoder
  annotations:
    alb.ingress.kubernetes.io/scheme: internal
    alb.ingress.kubernetes.io/target-type: ip
    alb.ingress.kubernetes.io/healthcheck-path: /healthz
    alb.ingress.kubernetes.io/listen-ports: '[{"HTTPS":443}]'
    alb.ingress.kubernetes.io/certificate-arn: arn:aws:acm:...:certificate/...
spec:
  ingressClassName: alb
  rules:
  - host: geocoder.internal.example.com
    http:
      paths:
      - path: /
        pathType: Prefix
        backend:
          service:
            name: geocoder
            port:
              number: 3000
```

`/healthz` as the health-check target matches what we documented
in [`BUILD-DEPLOY.md § 5.4`](../BUILD-DEPLOY.md#54-target-group).

## HorizontalPodAutoscaler

The server is CPU-bound under load (mmap page-ins + JSON
encoding). A simple CPU HPA covers most cases:

```yaml
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: geocoder
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: StatefulSet
    name: geocoder
  minReplicas: 3
  maxReplicas: 20
  metrics:
  - type: Resource
    resource:
      name: cpu
      target:
        type: Utilization
        averageUtilization: 60
```

**Note**: StatefulSet scale-up provisions a new PVC per pod from
the snapshot. Each scale-up event costs ~$16/month in storage
(200 GB gp3) and takes ~30–60 s for EBS provisioning + first mmap
page-ins. Factor that into your scaling policy — CPU-driven HPA
works, but aggressive rapid-scale-up will churn storage. A
`behavior.scaleUp.policies` policy that caps additions to 2 pods
per minute is a sensible guardrail.

## Index refresh workflow

The day-to-day operator workflow:

1. **Upstream build refresh** — CI triggers
   `make ami-worldwide-build` on schedule (weekly is typical).
   The Packer job takes ~18–24 h, uploads to S3 under a
   timestamped prefix + a `latest/` alias.
2. **In-cluster refresh** — a CronJob runs once daily, checks if
   the S3 `latest/` prefix has changed since the last in-cluster
   snapshot (compare `SHA256SUMS` or the LastModified of the
   canonical files). If changed, it runs the refresh Job shown
   above, produces a new `VolumeSnapshot`, and updates
   `geocoder-index-current` to point at it.
3. **Rolling pod replacement** — the StatefulSet's PVC template
   references `geocoder-index-current`. *Existing* PVCs don't
   reprovision (they stick to whatever snapshot they were born
   from). To roll onto the new snapshot:
   - Delete one pod's PVC at a time (via a controlled script)
     and let the StatefulSet recreate it.
   - Or annotate the StatefulSet with a trigger and rely on a
     flux/argo controller to cascade.
   - Or use `kubectl rollout restart statefulset/geocoder`
     combined with a pre-stop hook that deletes the pod's PVC
     — simplest for small fleets.

**Pattern for larger fleets**: use an Argo Rollout or
custom controller that manages the snapshot-to-PVC promotion
with canary-style rollout. Beyond the scope of this doc.

## Cost analysis

Example: 10-replica deployment with a 200 GB index.

| Line item | Unit | Monthly |
|---|---|---:|
| EBS gp3 per pod | 200 GB × $0.08/GB | $16/pod |
| × 10 pods | | **$160** |
| EBS snapshot storage | ~50 GB dedup × $0.05/GB | $3 |
| Data transfer (S3 → cluster) | weekly refresh, ~7 GB (only the diff via `aws s3 sync`) | $1 |
| Compute (c6i.xlarge × 10, spot) | $0.04/hr × 730 × 10 | **$292** |
| ALB | 730 hrs + small LCUs | $25 |
| **Total (excluding worldwide-build cost)** | | **~$480** |

For comparison with the AMI serving pattern from
[`BUILD-DEPLOY.md`](../BUILD-DEPLOY.md):

- **AMI**: 2× c6i.large (~$130) + 2×20GB EBS (~$4) + ALB (~$25)
  = **~$165/mo**.
- **Kubernetes per-pod-PVC**: 10× c6i.xlarge + 10×200GB EBS + ALB
  = **~$480/mo**.

The kubernetes deployment is more expensive at the same traffic
level because we're over-provisioning storage (10× 200 GB gp3 =
2 TB of gp3 charged as 10 separate volumes). It's a real cost,
not a trick of accounting — the tradeoff you're making is
storage-for-simplicity.

**Cost mitigations worth considering**:

- If the index is < 50 GB (AU-only, say), gp3 at $4/pod is
  negligible; the per-pod cost disappears.
- EBS Snapshot-backed volumes are deduplicated block-level at AWS
  (you don't pay 10× for the same data on disk, but you *do* pay
  for the 10 live volumes). Only snapshot storage gets the dedup
  discount.
- For much larger fleets (100+ pods), EFS becomes cheaper per
  pod (shared storage cost) even after the mmap performance
  penalty. Benchmark before switching.
- Spot instances drop the compute line ~70%; a 10-replica fleet
  on spot costs ~$100/month in compute.

## Alternatives (briefly)

### EFS (shared ReadOnlyMany)

```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: geocoder-index-efs
provisioner: efs.csi.aws.com
parameters:
  provisioningMode: efs-ap
  fileSystemId: fs-0abcdef1234567890
  directoryPerms: "0755"
```

PVC access mode `ReadOnlyMany`; all pods mount the same EFS path.
Trade local-EBS latency for shared storage cost. Our workload is
mmap-heavy with a ~200 GB working set that benefits strongly from
the kernel page cache — EFS is likely to hurt p99. Benchmark
carefully if you adopt this.

### S3 Mountpoint (FUSE)

Not recommended for mmap. Fine for append-only write, bulk read,
or batch ETL. Our server mmaps every query; FUSE adds kernel-
userspace round trips per page fault.

### Local-SSD (ephemeral)

If your node pool uses instance types with NVMe SSDs (e.g.
`c6id.*`, `m6id.*`), you can bypass EBS entirely by materialising
the index onto `/mnt/nvme` during pod startup. Native performance,
no persistence (which is actually fine because the index is
replaceable). More complex to manage; you lose snapshot semantics.
Usually overkill for this workload.

## Security

- `securityContext` already locked down on the StatefulSet pod
  (non-root, read-only root FS, all caps dropped, seccomp default).
- Consider a `NetworkPolicy` restricting ingress to the Ingress
  controller's pods or a specific internal namespace.
- The service account for the refresh Job must have narrowly-
  scoped S3 IAM — `s3:GetObject` on exactly the upstream prefix
  and `s3:ListBucket` on the parent. No `PutObject`; the refresh
  Job is a consumer, not a producer.
- EBS volumes are encrypted at rest via the StorageClass
  (`encrypted: "true"`). Snapshots inherit the encryption.

## Troubleshooting

| Symptom | First check |
|---|---|
| Pods stuck in `Pending` with `VolumeAttachLimitExceeded` | Each EC2 instance has a hard limit on EBS volumes (~30–40 depending on instance type). Spread pods across more nodes, or switch to instance types with higher volume limits. |
| Pods start but `/healthz` hangs for minutes | mmap cold-start page-in on a large index. Expected on first boot of a fresh PVC. Bump `readinessProbe.initialDelaySeconds` to 30–60 s. |
| Pods attach but `aws s3 sync` fails in the refresh Job | IRSA / Pod Identity not configured for the Job's ServiceAccount. Check `aws sts get-caller-identity` inside the Job pod. |
| `VolumeSnapshot` stuck in `readyToUse: false` | EBS CSI driver is still taking the snapshot. Large volumes (> 100 GB) take 5–10 minutes. If > 30 minutes, check the driver's controller pod logs. |
| Pods ready but queries return no country_code for a known country | Index was built without the WoF country fallback. Rebuild the worldwide index with `wof-importer` before FST; see [`worldwide-build.md`](worldwide-build.md). |
| Inconsistent results across pods | Pods born from different snapshots. Force a StatefulSet rollout once the canonical `geocoder-index-current` snapshot is updated. |

## ArgoCD-native deployment

If your cluster is already GitOps-managed with ArgoCD, the pattern
above maps cleanly but the **snapshot-name rotation** is the one
piece that doesn't fit pure declarative sync. The rest (Service,
Ingress, StatefulSet, StorageClass, VolumeSnapshotClass, HPA) is
ordinary application manifests Argo tracks without fuss.

### Repo layout

Standard app-of-apps with the geocoder components split so sync
order is obvious:

```
gitops-repo/
├── apps/
│   └── geocoder.yaml              # Argo Application pointing at manifests/
└── manifests/geocoder/
    ├── namespace.yaml
    ├── storage.yaml               # StorageClass + VolumeSnapshotClass
    ├── service.yaml
    ├── ingress.yaml
    ├── statefulset.yaml           # references snapshot via kustomize overlay
    ├── hpa.yaml
    ├── refresh-cronjob.yaml       # in-cluster snapshot refresh pipeline
    └── kustomization.yaml
```

### The `Application` manifest

```yaml
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: geocoder
  namespace: argocd
  finalizers:
  - resources-finalizer.argocd.argoproj.io
spec:
  project: default
  source:
    repoURL: https://github.com/my-org/gitops.git
    path: manifests/geocoder
    targetRevision: HEAD
  destination:
    server: https://kubernetes.default.svc
    namespace: geocoder
  syncPolicy:
    automated:
      prune: true
      selfHeal: true
    syncOptions:
    - CreateNamespace=true
    - ServerSideApply=true
    - RespectIgnoreDifferences=true
  # The snapshot's dataSource field is rotated by a CronJob (not git).
  # Tell Argo to ignore drift there so selfHeal doesn't fight the
  # refresh pipeline.
  ignoreDifferences:
  - group: apps
    kind: StatefulSet
    jqPathExpressions:
    - '.spec.volumeClaimTemplates[0].spec.dataSource'
    - '.spec.volumeClaimTemplates[0].spec.dataSourceRef'
```

`ignoreDifferences` is the key bit: the rotation pipeline
mutates `spec.volumeClaimTemplates[0].spec.dataSource` outside of
git, and without an ignore entry Argo's self-heal would
continuously revert to the stale snapshot reference.

### Sync waves for the refresh pipeline

Three phases, coordinated with sync-wave annotations:

```yaml
# Wave 0 — VolumeSnapshotClass + StorageClass (pre-existing or first sync)
annotations:
  argocd.argoproj.io/sync-wave: "0"
---
# Wave 5 — the refresh CronJob itself.
annotations:
  argocd.argoproj.io/sync-wave: "5"
---
# Wave 10 — StatefulSet. Comes up only after the snapshot
# named `geocoder-index-current` exists (either pre-bootstrapped
# or produced by the first manual run of the refresh Job).
annotations:
  argocd.argoproj.io/sync-wave: "10"
```

For the very first rollout, seed `geocoder-index-current` manually
(kubectl apply a VolumeSnapshot pointing at a one-off
`geocoder-index-builder` PVC hydrated from S3), then let Argo
take over.

### Snapshot rotation — the one awkward bit

Argo is declarative; snapshot rotation is imperative. Two
patterns that play nicely with ArgoCD:

**Pattern A: Argo Workflows for the refresh, Argo CD for the app.**
Wire the in-cluster refresh pipeline as an Argo Workflow that:

1. Provisions a builder PVC.
2. Runs `aws s3 sync` against the upstream S3 prefix.
3. Snapshots the PVC as `geocoder-index-YYYY-MM-DD`.
4. Patches the live StatefulSet's `volumeClaimTemplates.dataSource.name`
   to point at the new snapshot (via `kubectl patch`).
5. Annotates the Argo Application with
   `argocd.argoproj.io/refresh: hard` so Argo picks up the drift.

A `CronWorkflow` runs this on a schedule (daily poll for S3
changes, noop if the `SHA256SUMS` hash matches the last applied
one). Requires Argo Workflows to be installed alongside Argo CD,
but since most teams running Argo CD already want workflows for
similar reasons this is usually a small addition.

**Pattern B: ApplicationSet with a snapshot-name generator.**
If you don't want Argo Workflows, use an ApplicationSet with a
List or Plugin generator that reads the "current snapshot" from a
ConfigMap maintained by the refresh Job:

```yaml
apiVersion: argoproj.io/v1alpha1
kind: ApplicationSet
metadata:
  name: geocoder
spec:
  generators:
  - plugin:
      configMapRef:
        name: geocoder-snapshot-plugin
  template:
    metadata:
      name: geocoder-{{.region}}
    spec:
      source:
        repoURL: https://github.com/my-org/gitops.git
        path: manifests/geocoder
        targetRevision: HEAD
        kustomize:
          patches:
          - target:
              kind: StatefulSet
              name: geocoder
            patch: |
              - op: replace
                path: /spec/volumeClaimTemplates/0/spec/dataSource/name
                value: {{.snapshot_name}}
      destination:
        server: https://kubernetes.default.svc
        namespace: geocoder
```

The plugin is a tiny HTTP service in-cluster that the Job updates
after each successful snapshot. When the plugin returns a new
`snapshot_name`, ApplicationSet re-renders the Application and
Argo sees a diff on the StatefulSet's `dataSource`, rolling the
pods one at a time.

Of the two, **pattern A (Argo Workflows) is the recommended
flow** because it keeps the snapshot refresh and the application
sync as separate controllers — you can fail, inspect, and retry
the refresh independently of the serving pods. Pattern B is only
worth it if you've committed to never running Argo Workflows.

### Rolling over existing pods onto a new snapshot

A subtle point on StatefulSet behaviour: changing
`volumeClaimTemplates[0].dataSource` on a StatefulSet does **not**
cause existing pods' PVCs to reprovision. The dataSource is only
consulted when a PVC is created; existing PVCs are unaffected.

To actually roll the fleet onto the new snapshot you need to
delete each pod's PVC (which causes the StatefulSet controller
to recreate it from the new dataSource on pod recreation). A
one-pod-at-a-time script or an Argo Workflow step does this:

```bash
# Per-pod rollover, ordered, with a 60 s bake time between each.
for i in $(seq 0 $((REPLICAS-1))); do
    kubectl delete pvc index-geocoder-$i --wait=false
    kubectl delete pod geocoder-$i --wait=true
    kubectl rollout status statefulset/geocoder --timeout=5m
    sleep 60
done
```

Wrap that in an Argo Workflow step that runs after the new
snapshot is ready, and you have the full refresh → snapshot →
rolling-restart pipeline in one Workflow.

### Image tag updates (ArgoCD Image Updater)

Separate from the snapshot pipeline: the `query-server` container
image is updated via ordinary CI and [Argo CD Image Updater](https://argocd-image-updater.readthedocs.io/)
works out of the box. Annotation pattern:

```yaml
metadata:
  annotations:
    argocd-image-updater.argoproj.io/image-list: geocoder=ghcr.io/pdeaudney/geocoder
    argocd-image-updater.argoproj.io/geocoder.update-strategy: semver
    argocd-image-updater.argoproj.io/write-back-method: git
```

Image updater writes the new tag back to git; ordinary Argo CD
sync picks it up; StatefulSet does a rolling pod replacement
(existing PVCs carry over — no snapshot churn, just the binary
updates). This is the cheap path for most releases; the
snapshot pipeline only fires when the underlying data changes.

### Rollback

Two planes, two rollback flows:

- **Binary-only rollback** (bad release of `query-server`):
  `git revert` the image-updater commit; Argo reconciles; pods
  roll onto the previous image using the same PVCs.
- **Data rollback** (new index introduced bad geocodes): point
  `geocoder-index-current` at a prior timestamped snapshot via
  `kubectl patch` (or a `rollback` Argo Workflow that takes the
  target snapshot name as a parameter); run the per-pod rollover
  script. We keep `deletionPolicy: Retain` on the
  VolumeSnapshotClass specifically to support this — rolling back
  a month of index drift is a one-command operation.

## What's NOT covered here

- Multi-region rollouts: replicate the S3 artifact to regional
  buckets and run one StatefulSet per cluster. Standard k8s
  pattern, not geocoder-specific.
- Canary deploys: use Argo Rollouts or Flagger with the Service
  and a second StatefulSet suffixed `-canary`. Not specific to
  this workload.
- Observability: the server doesn't emit Prometheus metrics
  directly yet (flagged in [`BUILD-DEPLOY.md`](../BUILD-DEPLOY.md)
  future work). Derive dashboards from the Ingress / Service
  controller's own metrics until that lands.
