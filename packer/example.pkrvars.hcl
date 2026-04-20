// Example variable file. Copy to `prod.pkrvars.hcl` (or similar),
// tweak, and pass via `packer build -var-file=prod.pkrvars.hcl`.

build_region = "ap-southeast-2"

copy_regions = [
  "us-east-1",
  "us-west-2",
  "eu-west-1",
]

index_s3_uri  = "s3://my-geocoder-artifacts/index/au-latest"
binary_s3_uri = "s3://my-geocoder-artifacts/bin/query-server-v0.1.0"

// IAM instance profile on the build-side EC2 instance. Minimal policy:
//   s3:GetObject on arn:aws:s3:::my-geocoder-artifacts/index/au-latest/*
//   s3:GetObject on arn:aws:s3:::my-geocoder-artifacts/bin/query-server-v0.1.0
//   s3:ListBucket on arn:aws:s3:::my-geocoder-artifacts (for `aws s3 sync`)
build_instance_profile = "geocoder-packer-builder"

// Bump if your index is big. AU alone is ~2 GB → 20 GB is plenty.
// Worldwide deployments will want 200-500 GB.
volume_size_gb = 20

instance_type = "c6i.large"
