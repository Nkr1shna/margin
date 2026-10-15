# The Ubuntu 24.04 guest image for Margin's Linux VMs on the Mac mini
# (docs/plan-linux-service.md §5): the CI runner (margin-ci) and the service guests
# (margin-dev, margin-prod) all start from it. Orchard re-clones a VM from its image on
# every restart, so everything a guest needs that isn't data lives here; data, config,
# secrets and the runner's registration live on the VM's host share.
#
#   packer init deploy/image/margin-linux.pkr.hcl
#   packer build -var vm_name=margin-linux-v1 deploy/image/margin-linux.pkr.hcl
#
# The result is a local Tart VM (`tart list`). CI (.github/workflows/image.yml) builds it
# the same way and publishes it as ghcr.io/nkr1shna/margin-linux:<date>-<sha> and :latest. Built the way Cirrus Labs
# builds its images (github.com/cirruslabs/macos-image-templates): Packer's tart-cli
# source clones a base image and runs the provisioning scripts in it.

packer {
  required_plugins {
    tart = {
      version = ">= 1.12.0"
      source  = "github.com/cirruslabs/tart"
    }
  }
}

# The Rust version rust-toolchain.toml pins, so the image carries the toolchain CI uses.
locals {
  rust_toolchain = regex("channel = \"([^\"]+)\"", file(abspath("${path.root}/../../rust-toolchain.toml")))[0]
}

variable "vm_name" {
  type    = string
  default = "margin-linux-v1"
}

source "tart-cli" "margin" {
  vm_base_name = "ghcr.io/cirruslabs/ubuntu:24.04"
  vm_name      = var.vm_name
  cpu_count    = 4
  memory_gb    = 4
  disk_size_gb = 40
  headless     = true
  ssh_username = "admin"
  ssh_password = "admin"
  ssh_timeout  = "300s"
}

build {
  sources = ["source.tart-cli.margin"]

  # System packages, tools and the GitHub runner (as root).
  provisioner "shell" {
    environment_vars = ["DEBIAN_FRONTEND=noninteractive", "MARGIN_IMAGE=${var.vm_name}-${formatdate("YYYYMMDDhhmm", timestamp())}"]
    execute_command  = "{{ .Vars }} sudo -E sh -eu '{{ .Path }}'"
    script           = "${path.root}/linux-system.sh"
  }

  # Rust for the admin user, who runs CI jobs: the version rust-toolchain.toml pins.
  provisioner "shell" {
    environment_vars = ["RUST_TOOLCHAIN=${local.rust_toolchain}"]
    execute_command  = "{{ .Vars }} sh -eu '{{ .Path }}'"
    script          = "${path.root}/linux-user.sh"
  }
}
