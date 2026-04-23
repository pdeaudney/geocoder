// Example inputs for the worldwide build. Copy to `worldwide.pkrvars.hcl`,
// fill in bucket / IAM names, then:
//
//   packer build -var-file=worldwide.pkrvars.hcl build-worldwide.pkr.hcl
//
// Expect ~18–24 hours wall-time on the default r8g.16xlarge, and
// ~$30–80 in compute + S3 upload + data-egress costs depending on
// whether you use spot pricing.

build_region = "us-east-1"

// Where the resulting index files land. Trailing slash required.
// Per-build results land under `${prefix}${timestamp}/`; a `latest/`
// alias mirrors the most recent successful build.
output_s3_prefix = "s3://my-geocoder-artifacts/index/worldwide/"

// IAM instance profile for the builder. Minimal policy:
//   Writing the output:
//     s3:PutObject, s3:DeleteObject on ${output_s3_prefix}*
//     (PutObjectAcl if the bucket uses default-object ACLs)
//     s3:ListBucket on the parent bucket (so aws s3 sync can diff).
//   Reading OpenAddresses data (when openaddresses_enabled=true):
//     s3:GetObject on arn:aws:s3:::v2.openaddresses.io/*
//     s3:ListBucket on arn:aws:s3:::v2.openaddresses.io
//     (Requester-Pays — this account pays the egress. ~$1-6 one-off.)
build_instance_profile = "geocoder-worldwide-builder"

// Source selection — change if you're running from a fork or tag.
git_repo_url = "https://github.com/pdeaudney/geocoder.git"
git_ref      = "main"

// Scope. `planet` pulls the 75 GB OSM PBF + 8.6 GB WoF admin.
// For a smaller scope set these to specific continents / countries:
//   osm_region = "europe"       (passed to scripts/download-region.sh)
//   wof_scope  = "gb fr de nl"  (space-separated ISO 3166-1 alpha-2)
osm_region = "planet"
wof_scope  = "planet"

// Optional license-gated sources. Leave unset to skip.
// gnaf_archive_url    = "https://data.gov.au/data/dataset/.../FILE.zip"
// maxmind_license_key = "XXXXXXXXXXXXX"
// openaddresses_enabled = true     // reads from s3://v2.openaddresses.io/ (Requester-Pays) via the builder's IAM role. Set to false to skip OA and rely on OSM+WoF only.
// openaddresses_sources = "all"     // or "au gb us fr de" for a narrower subset. Ignored when openaddresses_enabled is false.

// Hardware.
instance_type  = "r8g.16xlarge"   // Graviton 4: 64 vCPU / 512 GB
architecture   = "arm64"
volume_size_gb = 1024
