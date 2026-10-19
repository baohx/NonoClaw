#!/usr/bin/env bash
# Build `nonoclaw-vm-runtime:latest` — the privileged sidecar container that
# hosts firecracker on hosts without passwordless sudo (docker group only).
#
# Contents: firecracker binary + iproute2 (tap) + iptables (NAT) + dnsmasq
# (guest DHCP) — everything the entry script in vm/firecracker.rs needs.
#
# Prereq: release/vm/firecracker-1.9.1.tgz
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TGZ="${TGZ:-$REPO_ROOT/release/vm/firecracker-1.9.1.tgz}"
IMAGE="${IMAGE:-nonoclaw-vm-runtime:latest}"
MIRROR="${MIRROR:-mirrors.aliyun.com}"

[ -f "$TGZ" ] || { echo "firecracker tarball not found at $TGZ" >&2; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
tar -xzf "$TGZ" -C "$TMP"
FC_BIN="$(find "$TMP" -name 'firecracker-v*' -type f ! -name '*.debug' | head -1)"
[ -n "$FC_BIN" ] || { echo "no firecracker binary in tarball" >&2; exit 1; }
chmod +x "$FC_BIN"

cat > "$TMP/Dockerfile" <<EOF
FROM alpine:3.20
RUN sed -i "s|dl-cdn.alpinelinux.org|$MIRROR|g" /etc/apk/repositories \\
    && apk add --no-cache iproute2 iptables dnsmasq
COPY firecracker /assets/bin/firecracker
EOF
cp "$FC_BIN" "$TMP/firecracker"
docker build --network host -t "$IMAGE" "$TMP" >&2
echo "===RUNTIME_OK==="
