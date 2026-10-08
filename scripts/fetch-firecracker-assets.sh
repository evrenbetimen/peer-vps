#!/usr/bin/env bash
# Download everything `peervps serve --hypervisor firecracker` needs into one directory:
#   <dir>/firecracker              the VMM binary (latest release)
#   <dir>/vmlinux                  a guest kernel from Firecracker's CI bucket
#   <dir>/images/ubuntu-24.04.ext4 an Ubuntu root filesystem (key: root, user: root)
#
# Usage: scripts/fetch-firecracker-assets.sh [dir]     (default: ./.firecracker)
#        FC_VERSION=v1.13.1 scripts/fetch-firecracker-assets.sh   to pin a release
# Needs: curl, tar, unsquashfs (squashfs-tools), mkfs.ext4 (e2fsprogs), ssh-keygen.
set -euo pipefail

DIR="${1:-.firecracker}"
ARCH="$(uname -m)"
RELEASES="https://github.com/firecracker-microvm/firecracker/releases"
BUCKET="https://s3.amazonaws.com/spec.ccfc.min"

mkdir -p "$DIR/images"
cd "$DIR"

latest="${FC_VERSION:-}"
if [[ -z "$latest" ]]; then
  latest="$(curl -fsSL https://api.github.com/repos/firecracker-microvm/firecracker/releases/latest \
    | grep -m1 '"tag_name"' | sed -E 's/.*"(v[^"]+)".*/\1/')" || true
fi
[[ "$latest" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "could not determine the Firecracker release; set FC_VERSION" >&2; exit 1; }
echo "firecracker $latest ($ARCH)"
if [[ ! -x firecracker ]]; then
  curl -fsSL "$RELEASES/download/$latest/firecracker-$latest-$ARCH.tgz" | tar -xz
  mv "release-$latest-$ARCH/firecracker-$latest-$ARCH" firecracker
  rm -rf "release-$latest-$ARCH"
fi

ci="${latest%.*}"
list() { curl -fsSL "$BUCKET/?prefix=firecracker-ci/$ci/$ARCH/$1&list-type=2" | grep -oE "<Key>[^<]+</Key>" | sed -E 's#</?Key>##g'; }

if [[ ! -f vmlinux ]]; then
  key="$(list vmlinux- | grep -E 'vmlinux-[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -1)"
  echo "kernel $key"
  curl -fsSL -o vmlinux "$BUCKET/$key"
fi

if [[ ! -f images/ubuntu-24.04.ext4 ]]; then
  key="$(list ubuntu- | grep -E 'ubuntu-[0-9]+\.[0-9]+\.squashfs$' | sort -V | tail -1)"
  echo "rootfs $key"
  curl -fsSL -o ubuntu.squashfs "$BUCKET/$key"
  rm -rf squashfs-root && unsquashfs -q -no-progress ubuntu.squashfs
  # Key-based root login so `peervps` can reach the guest over SSH.
  [[ -f id_ed25519 ]] || ssh-keygen -q -t ed25519 -N "" -f id_ed25519
  mkdir -p squashfs-root/root/.ssh
  cp id_ed25519.pub squashfs-root/root/.ssh/authorized_keys
  mkfs.ext4 -q -d squashfs-root -F images/ubuntu-24.04.ext4 1G
  rm -rf squashfs-root ubuntu.squashfs
fi

cat <<MSG

Assets ready in $(pwd). Start a node that boots real MicroVMs (needs /dev/kvm):

  cargo run -p peervps-cli -- serve --hypervisor firecracker \\
    --fc-binary $(pwd)/firecracker --fc-kernel $(pwd)/vmlinux --fc-images $(pwd)/images
MSG
