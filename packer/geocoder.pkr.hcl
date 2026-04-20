// Packer config: build an Ubuntu 24.04 AMI pre-loaded with the
// self-hosted geocoder binary and the mmap-backed index files
// synced from S3. Build in one region, copy to N others.
//
// Usage:
//   cd packer
//   packer init .
//   packer build -var-file=example.pkrvars.hcl geocoder.pkr.hcl
//
// Required variables (see example.pkrvars.hcl):
//   index_s3_uri        s3://bucket/prefix containing the index files
//   binary_s3_uri       s3://bucket/path/query-server (pre-built binary)
//   build_instance_profile IAM instance profile with s3:GetObject access
//   build_region        region to build in (e.g. ap-southeast-2)
//   copy_regions        list of regions to copy the AMI into
//
// The running builder instance needs s3:GetObject on both URIs above.
// Your local Packer credentials need standard EC2/AMI permissions.

packer {
  required_plugins {
    amazon = {
      version = ">= 1.3.0"
      source  = "github.com/hashicorp/amazon"
    }
  }
}

variable "build_region" {
  type        = string
  description = "AWS region to build the AMI in."
}

variable "copy_regions" {
  type        = list(string)
  default     = []
  description = "Additional regions to copy the AMI into after build."
}

variable "ami_name_prefix" {
  type    = string
  default = "geocoder"
}

variable "instance_type" {
  type        = string
  default     = "c6i.large"
  description = "Build instance type. Needs enough RAM/network for the S3 sync."
}

variable "volume_size_gb" {
  type        = number
  default     = 20
  description = "Root EBS size in GB. Must fit OS (~4 GB) + the whole index."
}

variable "index_s3_uri" {
  type        = string
  description = "S3 URI prefix holding the index files (no trailing slash)."
}

variable "binary_s3_uri" {
  type        = string
  description = "S3 URI of the pre-built query-server binary."
}

variable "build_instance_profile" {
  type        = string
  description = "IAM instance profile attached to the builder EC2 instance. Must allow s3:GetObject on index_s3_uri and binary_s3_uri."
}

variable "index_install_dir" {
  type    = string
  default = "/var/lib/geocoder/index"
}

variable "binary_install_path" {
  type    = string
  default = "/usr/local/bin/query-server"
}

variable "service_user" {
  type    = string
  default = "geocoder"
}

variable "bind_addr" {
  type        = string
  default     = "0.0.0.0:3000"
  description = "HTTP bind address baked into the default systemd unit. Override at deploy time via /etc/default/geocoder if needed."
}

// Canonical's Ubuntu 24.04 LTS (Noble) amd64 AMI, resolved at build time.
// owner-id 099720109477 is Canonical's official AWS account.
data "amazon-ami" "ubuntu_2404" {
  filters = {
    name                = "ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*"
    root-device-type    = "ebs"
    virtualization-type = "hvm"
    architecture        = "x86_64"
  }
  owners      = ["099720109477"]
  most_recent = true
  region      = var.build_region
}

locals {
  timestamp = regex_replace(timestamp(), "[^0-9]", "")
  ami_name  = "${var.ami_name_prefix}-ubuntu2404-${local.timestamp}"
}

source "amazon-ebs" "geocoder" {
  region                      = var.build_region
  ami_name                    = local.ami_name
  ami_description             = "Self-hosted geocoder on Ubuntu 24.04 LTS with index baked in from ${var.index_s3_uri}"
  instance_type               = var.instance_type
  source_ami                  = data.amazon-ami.ubuntu_2404.id
  ssh_username                = "ubuntu"
  iam_instance_profile        = var.build_instance_profile
  associate_public_ip_address = true
  ami_regions                 = var.copy_regions

  launch_block_device_mappings {
    device_name           = "/dev/sda1"
    volume_size           = var.volume_size_gb
    volume_type           = "gp3"
    delete_on_termination = true
  }

  tags = {
    Name        = local.ami_name
    Base        = "ubuntu-24.04-lts"
    IndexSource = var.index_s3_uri
    BuildTool   = "packer"
  }
}

build {
  name    = "geocoder-ami"
  sources = ["source.amazon-ebs.geocoder"]

  // 1. Wait for cloud-init to finish; apt-get otherwise races the
  //    unattended-upgrades hook and fails with a lock error.
  provisioner "shell" {
    inline = [
      "cloud-init status --wait",
      "sudo apt-get update -y",
      "sudo apt-get install -y --no-install-recommends awscli ca-certificates",
    ]
  }

  // 2. Create the service user and the mmap directory.
  provisioner "shell" {
    environment_vars = [
      "SERVICE_USER=${var.service_user}",
      "INDEX_DIR=${var.index_install_dir}",
    ]
    inline = [
      "sudo useradd --system --home-dir /var/lib/geocoder --shell /usr/sbin/nologin $SERVICE_USER || true",
      "sudo mkdir -p $INDEX_DIR",
    ]
  }

  // 3. Download binary and index from S3 using the build instance's IAM
  //    role. `aws s3 sync` is idempotent and handles resumes; `aws s3 cp`
  //    for the single binary.
  provisioner "shell" {
    environment_vars = [
      "INDEX_S3_URI=${var.index_s3_uri}",
      "BINARY_S3_URI=${var.binary_s3_uri}",
      "INDEX_DIR=${var.index_install_dir}",
      "BINARY_PATH=${var.binary_install_path}",
    ]
    inline = [
      "echo 'Downloading index from '$INDEX_S3_URI' to '$INDEX_DIR",
      "sudo aws s3 sync --no-progress --delete $INDEX_S3_URI $INDEX_DIR",
      "echo 'Downloading binary from '$BINARY_S3_URI' to '$BINARY_PATH",
      "sudo aws s3 cp --no-progress $BINARY_S3_URI $BINARY_PATH",
      "sudo chmod 0755 $BINARY_PATH",
      "sudo chown -R ${var.service_user}:${var.service_user} $INDEX_DIR",
    ]
  }

  // 4. Drop the systemd unit + defaults file.
  provisioner "file" {
    source      = "files/geocoder.service"
    destination = "/tmp/geocoder.service"
  }
  provisioner "file" {
    source      = "files/geocoder.env"
    destination = "/tmp/geocoder.env"
  }

  provisioner "shell" {
    environment_vars = [
      "SERVICE_USER=${var.service_user}",
      "INDEX_DIR=${var.index_install_dir}",
      "BINARY_PATH=${var.binary_install_path}",
      "BIND_ADDR=${var.bind_addr}",
    ]
    inline = [
      // Render the unit file with build-time paths substituted in.
      "sudo install -m 0644 /tmp/geocoder.service /etc/systemd/system/geocoder.service",
      "sudo install -m 0644 /tmp/geocoder.env /etc/default/geocoder",
      "sudo sed -i \"s|@SERVICE_USER@|$SERVICE_USER|g\" /etc/systemd/system/geocoder.service",
      "sudo sed -i \"s|@BINARY_PATH@|$BINARY_PATH|g\" /etc/systemd/system/geocoder.service",
      "sudo sed -i \"s|@INDEX_DIR@|$INDEX_DIR|g\" /etc/systemd/system/geocoder.service",
      "sudo sed -i \"s|@BIND_ADDR@|$BIND_ADDR|g\" /etc/default/geocoder",
      "sudo sed -i \"s|@INDEX_DIR@|$INDEX_DIR|g\" /etc/default/geocoder",
      "sudo systemctl daemon-reload",
      "sudo systemctl enable geocoder.service",
    ]
  }

  // 5. Tidy up before snapshot: fstrim claims back unallocated blocks so
  //    the resulting AMI is smaller; sync flushes caches.
  provisioner "shell" {
    inline = [
      "sudo rm -rf /var/lib/apt/lists/*",
      "sudo apt-get clean",
      "sudo fstrim -av || true",
      "sync",
    ]
  }
}
