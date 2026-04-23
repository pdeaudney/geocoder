// Packer config: one-shot EC2 job that builds the worldwide geocoder
// index from source and uploads the resulting .bin files to S3.
//
// The instance is a large-memory Graviton box because libosmium's
// single-threaded handler is the bottleneck and the planet build
// needs ~256 GB RAM peak (we profiled 15 GB RSS + 23 GB file-backed
// node cache for a 20 GB 5-country PBF set; planet is 75 GB, scales
// super-linearly from there — see docs/research/libosmium-parallelisation.md
// for why we can't just add more build threads).
//
// Index files are `#[repr(C)]` structs with no usize/isize and
// native-endian primitives; since all modern EC2 instances (Graviton
// and Intel/AMD) are little-endian, a build produced on r8g.16xlarge
// serves cleanly from any c7g / r7g / c6i / m6i / r6i instance.
//
// Usage:
//   cd packer
//   packer init .
//   packer build -var-file=worldwide.pkrvars.hcl build-worldwide.pkr.hcl
//
// Expected wall-time: ~18–24 hours end-to-end for planet. Cost
// envelope on r8g.16xlarge on-demand (~$3/hr) ≈ $60. Spot is ~$1/hr.
//
// The resulting AMI is saved for audit; the *real* deliverable is
// the index uploaded to `output_s3_prefix`, which serving instances
// then mirror into their local mmap dir.

packer {
  required_plugins {
    amazon = {
      version = ">= 1.3.0"
      source  = "github.com/hashicorp/amazon"
    }
  }
}

// --- Required variables ---

variable "build_region" {
  type        = string
  description = "AWS region to run the build in. Pick one close to the Geofabrik/OSM mirror you expect to hit to shave off PBF download time."
}

variable "output_s3_prefix" {
  type        = string
  description = "s3://bucket/path/ (trailing slash) where the build's resulting index files will be uploaded. The running builder needs s3:PutObject on every key under this prefix."
}

variable "build_instance_profile" {
  type        = string
  description = "IAM instance profile attached to the builder. Must allow s3:PutObject (and s3:DeleteObject if you enable the `aws s3 sync --delete` flag on output) under output_s3_prefix."
}

// --- Source selection ---

variable "git_repo_url" {
  type        = string
  default     = "https://github.com/pdeaudney/geocoder.git"
  description = "Repo to clone on the builder."
}

variable "git_ref" {
  type        = string
  default     = "main"
  description = "Branch / tag / SHA to build from."
}

// --- Scope knobs ---

variable "osm_region" {
  type        = string
  default     = "planet"
  description = "Which OSM extract(s) to download. `planet` uses the canonical planet-latest.osm.pbf; any other value is passed verbatim to scripts/download-region.sh (which accepts europe, north-america, asia, africa, australia, etc.). Defaults to planet for worldwide coverage."
}

variable "wof_scope" {
  type        = string
  default     = "planet"
  description = "Whose on First admin scope. `planet` fetches the single ~8.6 GB combined SQLite. Otherwise treated as a space-separated list of ISO 3166-1 alpha-2 codes (e.g. `au nz gb`) and per-country SQLite files are fetched instead."
}

variable "openaddresses_enabled" {
  type        = bool
  default     = true
  description = "Whether to fetch + index OpenAddresses data. Uses the build instance's IAM role (no additional token needed) to read from s3://v2.openaddresses.io/ which is Requester-Pays. Egress costs ~$1-6 for the global scope depending on region. Set to false to skip OA entirely."
}

variable "openaddresses_sources" {
  type        = string
  default     = "all"
  description = "Space-separated list of OA source prefixes (typically ISO 3166-1 alpha-2 codes like 'au gb us'), or 'all' for the full global collection (~66 GB, ~$1-6 egress). Ignored when openaddresses_enabled is false."
}

variable "gnaf_archive_url" {
  type        = string
  default     = ""
  description = "HTTPS URL to a license-accepted G-NAF ZIP on data.gov.au. Leave empty to skip the AU-authoritative address layer (forward geocoding still works via OSM; reverse loses per-address postcode precision in AU)."
  sensitive   = true
}

variable "maxmind_license_key" {
  type        = string
  default     = ""
  description = "MaxMind GeoLite2 license key. Leave empty to skip /geocode/ip support on the resulting index."
  sensitive   = true
}

// --- Hardware ---

variable "instance_type" {
  type        = string
  default     = "r8g.16xlarge"
  description = "Build instance type. Defaults to Graviton 4 r8g.16xlarge: 64 vCPU / 512 GB RAM / aarch64. The RAM headroom matters more than CPU count because libosmium serializes handler dispatch. r7g.16xlarge works too and is slightly cheaper."
}

variable "architecture" {
  type        = string
  default     = "arm64"
  description = "ami arch filter. `arm64` pairs with r8g/r7g/c7g instance types; `x86_64` with r6i/c6i/m6i."
  validation {
    condition     = contains(["arm64", "x86_64"], var.architecture)
    error_message = "Architecture must be either arm64 or x86_64."
  }
}

variable "volume_size_gb" {
  type        = number
  default     = 1024
  description = "Root EBS size. Planet build: ~75 GB PBF + ~1.7 TB transient osmium node cache + ~200 GB final index. 1 TB gp3 is the sane minimum; bump to 2 TB if you're doing planet + WoF + OA + G-NAF concurrently."
}

// --- AMI metadata ---

variable "ami_name_prefix" {
  type    = string
  default = "geocoder-worldwide-build"
}

variable "copy_regions" {
  type        = list(string)
  default     = []
  description = "Additional regions the resulting AMI gets copied into. Usually empty — the AMI is auxiliary; the real artifact is what landed in S3."
}

locals {
  timestamp = regex_replace(timestamp(), "[^0-9]", "")
  ami_name  = "${var.ami_name_prefix}-${local.timestamp}"
}

// Canonical's Ubuntu 24.04 LTS for the configured architecture.
data "amazon-ami" "ubuntu_2404" {
  filters = {
    name                = "ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-${var.architecture}-server-*"
    root-device-type    = "ebs"
    virtualization-type = "hvm"
    architecture        = var.architecture
  }
  owners      = ["099720109477"]
  most_recent = true
  region      = var.build_region
}

source "amazon-ebs" "worldwide_build" {
  region                      = var.build_region
  ami_name                    = local.ami_name
  ami_description             = "One-shot worldwide index build on ${var.instance_type}. Source: ${var.git_repo_url}@${var.git_ref}. Output: ${var.output_s3_prefix}"
  instance_type               = var.instance_type
  source_ami                  = data.amazon-ami.ubuntu_2404.id
  ssh_username                = "ubuntu"
  ssh_timeout                 = "20m"
  iam_instance_profile        = var.build_instance_profile
  associate_public_ip_address = true
  ami_regions                 = var.copy_regions

  // The build provisioners run for many hours. Packer defaults on SSH
  // handshake/connection retries are enough; but the base communicator
  // timeout matters on first-boot cloud-init.

  launch_block_device_mappings {
    device_name           = "/dev/sda1"
    volume_size           = var.volume_size_gb
    volume_type           = "gp3"
    iops                  = 6000
    throughput            = 500
    delete_on_termination = true
  }

  tags = {
    Name        = local.ami_name
    Purpose     = "worldwide-index-builder"
    GitRef      = var.git_ref
    OutputS3    = var.output_s3_prefix
    OsmRegion   = var.osm_region
  }
}

build {
  name    = "worldwide-index-build"
  sources = ["source.amazon-ebs.worldwide_build"]

  // Stage 1: install toolchain + cloud-init wait + basic prep.
  // Broken into its own provisioner so a failure here reports quickly
  // rather than ~20 hours into a bad image.
  provisioner "shell" {
    inline = [
      "cloud-init status --wait",
      "sudo DEBIAN_FRONTEND=noninteractive apt-get update -y",
      "sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \\",
      "    build-essential cmake g++ \\",
      "    libosmium2-dev libs2-dev libprotozero-dev \\",
      "    protobuf-compiler \\",
      "    zlib1g-dev libbz2-dev libexpat1-dev liblz4-dev \\",
      "    git curl ca-certificates \\",
      "    awscli \\",
      "    bzip2 unzip \\",
      "    osmium-tool",
      // Rust via rustup so we don't depend on the distro's stale version.
      "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable",
      "echo 'source $HOME/.cargo/env' >> $HOME/.bashrc",
    ]
  }

  // Stage 2: clone + build. Build takes ~10 min; failures here are
  // cheap compared to waiting 15 hr before noticing a compile error.
  provisioner "shell" {
    environment_vars = [
      "GIT_REPO=${var.git_repo_url}",
      "GIT_REF=${var.git_ref}",
    ]
    inline = [
      "set -e",
      "cd $HOME",
      "git clone --depth 1 --branch $GIT_REF $GIT_REPO geocoder",
      "cd geocoder",
      "mkdir -p build",
      "cd build && cmake ../builder && make -j$(nproc)",
      "cd ..",
      "source $HOME/.cargo/env",
      "cargo build --release -p query-server --bin build-forward-index --bin build-autocomplete-fst --bin build-gnaf-index --bin build-postcode-lookup --bin build-openaddresses-index",
      "cargo build --release -p wof-importer",
    ]
  }

  // Stage 3: fetch data (longest single step; PBF + WoF dominate).
  // Drops a marker file so a manual re-run of stage 4 can skip this.
  provisioner "shell" {
    environment_vars = [
      "OSM_REGION=${var.osm_region}",
      "WOF_SCOPE=${var.wof_scope}",
      "MAXMIND_LICENSE_KEY=${var.maxmind_license_key}",
      "GNAF_ARCHIVE_URL=${var.gnaf_archive_url}",
      "OPENADDRESSES_ENABLED=${var.openaddresses_enabled}",
      "OPENADDRESSES_SOURCES=${var.openaddresses_sources}",
    ]
    inline = [
      "set -e",
      "cd $HOME/geocoder",
      "mkdir -p data/pbf test-data",
      "./scripts/download-region.sh $OSM_REGION ./data/pbf",
      // WoF: planet uses the single combined admin file; otherwise
      // the space-separated list is forwarded to fetch-test-data.sh.
      "if [ \"$WOF_SCOPE\" = \"planet\" ]; then",
      "    echo '==> fetching whosonfirst-data-admin-latest (~8.6 GB bz2)'",
      "    curl -fSL -o test-data/wof-planet.db.bz2 https://data.geocode.earth/wof/dist/sqlite/whosonfirst-data-admin-latest.db.bz2",
      "    bzip2 -d test-data/wof-planet.db.bz2",
      "    mv test-data/wof-planet.db test-data/whosonfirst-data-admin-planet-latest.db",
      "else",
      "    WOF_COUNTRIES=\"$WOF_SCOPE\" ./scripts/fetch-test-data.sh --all-corpora",
      "fi",
      // OpenAddresses — uses the build instance's IAM role to read
      // from s3://v2.openaddresses.io/ (Requester-Pays). No extra
      // tokens or signup steps needed; we're already on EC2.
      "if [ \"$OPENADDRESSES_ENABLED\" = \"true\" ]; then",
      "    OA_SOURCES=\"$OPENADDRESSES_SOURCES\" OA_OUTPUT_DIR=./data/openaddresses \\",
      "        ./scripts/fetch-openaddresses.sh",
      "fi",
      "touch /tmp/fetch-complete",
    ]
  }

  // Stage 4: the build itself. PBF → reverse index → forward index
  // → FST → WoF import → (optional) G-NAF / OpenAddresses / MaxMind.
  // Each sub-step logs elapsed time so the post-run forensic is easy.
  provisioner "shell" {
    environment_vars = [
      "OSM_REGION=${var.osm_region}",
      "GNAF_ARCHIVE_URL=${var.gnaf_archive_url}",
    ]
    inline = [
      "set -e",
      "cd $HOME/geocoder",
      "mkdir -p data/index-worldwide",
      // Reverse index. Using all available PBFs in ./data/pbf/.
      "echo '=== build-index (reverse) ==='",
      "time ./build/build-index data/index-worldwide data/pbf/*.osm.pbf",
      // Forward (tantivy).
      "echo '=== build-forward-index ==='",
      "time ./target/release/build-forward-index data/index-worldwide",
      // WoF country fallback (must run before FST so FST picks up the
      // country_codes our OSM admin scan may have missed).
      "echo '=== wof-importer ==='",
      "time ./target/release/wof-importer ./test-data data/index-worldwide",
      // Autocomplete FST.
      "echo '=== build-autocomplete-fst ==='",
      "time ./target/release/build-autocomplete-fst data/index-worldwide --layout both",
      // Optional OpenAddresses. Fires only when stage 3 pulled
      // anything into data/openaddresses/. Builder skips AU
      // automatically (G-NAF is authoritative).
      "if [ -d data/openaddresses ] && [ -n \"$(ls -A data/openaddresses 2>/dev/null)\" ]; then",
      "    echo '=== OpenAddresses unzip + build ==='",
      "    for zip in data/openaddresses/*.cache.zip; do",
      "        [ -f \"$zip\" ] || continue",
      "        dest=\"data/openaddresses/$(basename \"$zip\" .cache.zip)\"",
      "        mkdir -p \"$dest\"",
      "        unzip -oq \"$zip\" -d \"$dest\" || true",
      "    done",
      "    time ./target/release/build-openaddresses-index data/openaddresses data/index-worldwide --skip au || echo 'openaddresses step failed — continuing'",
      "else",
      "    echo '=== OpenAddresses skipped (no data/openaddresses/ contents) ==='",
      "fi",
      // Optional G-NAF (AU-authoritative). Skip silently if not provided.
      "if [ -n \"$GNAF_ARCHIVE_URL\" ]; then",
      "    echo '=== G-NAF import ==='",
      "    mkdir -p data/gnaf",
      "    curl -fSL -o /tmp/gnaf.zip \"$GNAF_ARCHIVE_URL\"",
      "    unzip -q /tmp/gnaf.zip -d data/gnaf/",
      "    time ./target/release/build-gnaf-index data/gnaf data/index-worldwide || echo 'gnaf step failed — continuing'",
      "    time ./target/release/build-postcode-lookup data/gnaf data/index-worldwide || echo 'postcode-lookup step failed — continuing'",
      "fi",
    ]
  }

  // Stage 5: upload to S3. Atomic relative to a consumer since the
  // runtime loader checks existence-of-all-files at Index::load and
  // fails cleanly on a partial set. For extra safety we upload to a
  // timestamped prefix and emit a `latest` marker last.
  provisioner "shell" {
    environment_vars = [
      "OUTPUT_S3_PREFIX=${var.output_s3_prefix}",
      "STAMP=${local.timestamp}",
    ]
    inline = [
      "set -e",
      "cd $HOME/geocoder",
      "echo '=== computing checksums ==='",
      "(cd data/index-worldwide && find . -type f -not -name SHA256SUMS | sort | xargs shasum -a 256) > data/index-worldwide/SHA256SUMS",
      "echo '=== uploading to s3 ==='",
      "aws s3 sync --no-progress data/index-worldwide/ \"$OUTPUT_S3_PREFIX$STAMP/\"",
      // Serving-side convenience: alias the timestamped dir as `latest/`.
      "aws s3 sync --no-progress --delete data/index-worldwide/ \"$${OUTPUT_S3_PREFIX}latest/\"",
      "echo '=== done ==='",
      "du -sh data/index-worldwide",
    ]
  }

  // Stage 6: trim the AMI before snapshot. The index is on disk for
  // the duration of the build, and the snapshot would capture it
  // unless we clean up. AMI size matters less than you'd think — the
  // caller shouldn't use this AMI to serve traffic anyway (serving
  // uses `packer/geocoder.pkr.hcl`).
  provisioner "shell" {
    inline = [
      "sudo rm -rf /home/ubuntu/geocoder/data/pbf",
      "sudo rm -rf /home/ubuntu/geocoder/test-data",
      "sudo apt-get clean",
      "sudo rm -rf /var/lib/apt/lists/*",
      "sudo fstrim -av || true",
      "sync",
    ]
  }
}
