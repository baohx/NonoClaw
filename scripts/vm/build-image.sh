#!/usr/bin/env bash
# Build the shared VM base image assets (kernel + rootfs) for NonoClaw microVMs.
#
# Runs entirely inside docker containers (no host root needed):
#   stage 1: convert/copy the Debian 12 nocloud qcow2 to a raw ext4 rootfs,
#            enlarge it, and chroot-configure it via systemd-nspawn-like setup;
#   stage 2: extract kernel+initrd for direct-kernel-boot, bake in the global
#            SSH pubkey, chromium, CDP systemd unit.
#
# Outputs (in $ASSETS_DIR, default ~/.nonoclaw/vm):
#   rootfs-bare.ext4      — base disk, BARE ext4 (no partition table). Instances
#                           copy it; Firecracker boots it with root=/dev/vda.
#   kernel.img            — Firecracker CI ELF vmlinux (NOT the Debian bzImage:
#                           Firecracker's loader rejects the LZMA bzImage with
#                           InvalidElfMagicNumber). Sourced from release/vm/.
#   kernel-qemu.img       — Debian distro bzImage (full virtio-pci + PCI + ACPI)
#                           for the QEMU backend; the FC vmlinux can't find the
#                           disk under QEMU. QEMU's loader accepts the bzImage.
#   initrd.img            — matching Debian initrd
#   ssh_key / ssh_key.pub — global VM keypair
#
# Prereqs (both in release/vm/):
#   debian-12-nocloud.qcow2 — https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-nocloud-amd64.qcow2
#   vmlinux-fc.bin          — Firecracker CI uncompressed ELF kernel, e.g.
#     https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/x86_64/vmlinux-6.1.102
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SRC_QCOW2="${SRC_QCOW2:-$REPO_ROOT/release/vm/debian-12-nocloud.qcow2}"
FC_KERNEL="${FC_KERNEL:-$REPO_ROOT/release/vm/vmlinux-fc.bin}"
ASSETS_DIR="${ASSETS_DIR:-$HOME/.nonoclaw/vm}"
SIZE_GB="${SIZE_GB:-3}"
MIRROR="${MIRROR:-mirrors.aliyun.com}"
WORK="$REPO_ROOT/release/vm/build"
CONTAINER_BUILD_IMAGE="debian:12"
DOCKER="${DOCKER:-docker}"

log() { printf '\033[1;36m[vm-image]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[vm-image] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

[ -f "$SRC_QCOW2" ] || die "source qcow2 not found at $SRC_QCOW2"
command -v "$DOCKER" >/dev/null || die "docker not available"
mkdir -p "$ASSETS_DIR" "$WORK"

# ---------------------------------------------------------------------------
# Global SSH keypair (baked into the base image; all instances share it)
# ---------------------------------------------------------------------------
if [ ! -f "$ASSETS_DIR/ssh_key" ]; then
    log "generating global VM ed25519 key"
    ssh-keygen -q -t ed25519 -N "" -C "nonoclaw-vm" -f "$ASSETS_DIR/ssh_key"
fi
PUBKEY="$(cat "$ASSETS_DIR/ssh_key.pub")"

# ---------------------------------------------------------------------------
# Stage 1+2 inside a privileged container (needs loop mounts)
# ---------------------------------------------------------------------------
log "building rootfs in privileged $CONTAINER_BUILD_IMAGE container"
[ -f "$FC_KERNEL" ] || die "Firecracker ELF kernel not found at $FC_KERNEL (see header prereqs)"
"$DOCKER" run --rm -i --privileged --network host \
  -v "$SRC_QCOW2":/src.qcow2:ro \
  -v "$FC_KERNEL":/fc-kernel.bin:ro \
  -v "$ASSETS_DIR":/assets \
  -v "$WORK":/work \
  -e PUBKEY="$PUBKEY" -e SIZE_GB="$SIZE_GB" -e MIRROR="$MIRROR" \
  -e http_proxy="${http_proxy:-${HTTP_PROXY:-}}" \
  -e https_proxy="${https_proxy:-${HTTPS_PROXY:-}}" \
  -e no_proxy="${no_proxy:-${NO_PROXY:-}}" \
  "$CONTAINER_BUILD_IMAGE" bash -s <<'EOS'
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

# apt: fail fast on stalled mirror connections and retry, instead of hanging
# forever (the aliyun mirror intermittently stalls mid-transfer on large
# packages like chromium). 30s connection/data timeout, 3 retries.
cat > /etc/apt/apt.conf.d/99timeout <<'APTCONF'
Acquire::http::Timeout "30";
Acquire::https::Timeout "30";
Acquire::Retries "3";
APTCONF

# --- container self-setup (CN mirror) ---------------------------------------
sed -i "s|deb.debian.org|$MIRROR|g" /etc/apt/sources.list.d/*.sources 2>/dev/null \
  || sed -i "s|deb.debian.org|$MIRROR|g" /etc/apt/sources.list
apt-get -qq update >/dev/null
apt-get -qq install -y qemu-utils systemd-container fdisk >/dev/null

# --- convert to raw and enlarge ----------------------------------------------
qemu-img convert -f qcow2 -O raw /src.qcow2 /work/rootfs.raw
truncate -s "${SIZE_GB}G" /work/rootfs.raw

# Attach the raw image read/write for chroot configuration. The disk stays
# at its original 3G here; per-instance growth happens on first boot via the
# firstboot-growfs.service unit installed below (standard cloud-image pattern:
# host truncates the instance copy larger, guest grows p1 at boot).
# Partition 1 (root) starts at sector 262144 -> byte offset 262144*512.
# Mounting with an offset avoids loop partition scanning entirely (loop -P
# partition nodes are unreliable inside privileged containers).
P1_OFFSET=$((262144 * 512))
mkdir -p /mnt/vm
mount -t ext4 -o loop,offset=$P1_OFFSET /work/rootfs.raw /mnt/vm

# --- configure via chroot ----------------------------------------------------
# 1. apt sources -> CN mirror
if [ -f /mnt/vm/etc/apt/sources.list.d/*.sources ]; then
    sed -i "s|deb.debian.org|$MIRROR|g" /mnt/vm/etc/apt/sources.list.d/*.sources
else
    sed -i "s|deb.debian.org|$MIRROR|g" /mnt/vm/etc/apt/sources.list
fi

# 2. hostname, resolv, root ssh key
echo nonoclaw-vm > /mnt/vm/etc/hostname
mkdir -p /mnt/vm/root/.ssh
echo "$PUBKEY" > /mnt/vm/root/.ssh/authorized_keys
chmod 700 /mnt/vm/root/.ssh
chmod 600 /mnt/vm/root/.ssh/authorized_keys

# 3. serial console getty (ttyS0) — required for FC/QEMU -nographic
ln -sf /lib/systemd/system/serial-getty@.service \
      /mnt/vm/etc/systemd/system/getty.target.wants/serial-getty@ttyS0.service

# 4. DHCP on eth0 via systemd-networkd (works for both tap+dnsmasq and slirp)
mkdir -p /mnt/vm/etc/systemd/network
cat > /mnt/vm/etc/systemd/network/80-dhcp.network <<'NET'
[Match]
Name=eth0

[Network]
DHCP=yes
NET
ln -sf /lib/systemd/system/systemd-networkd.service \
      /mnt/vm/etc/systemd/system/multi-user.target.wants/systemd-networkd.service
ln -sf /lib/systemd/system/systemd-networkd-socket-proxyd.service \
      /mnt/vm/etc/systemd/system/sockets.target.wants/systemd-networkd-socket-proxyd.service 2>/dev/null || true

# remove cloud-init ifupdown leftovers that could race DHCP
rm -f /mnt/vm/etc/systemd/system/sys-subsystem-net-devices-eth0.device.wants/dhclient@eth0.service 2>/dev/null || true

# 5. chroot: openssh-server + chromium + tools
for f in proc sys dev; do mount --rbind /$f /mnt/vm/$f; done
mount -t proc proc /mnt/vm/proc 2>/dev/null || true

# Working DNS inside the chroot BEFORE any apt call. The Debian image ships an
# empty/stale resolv.conf, so apt-get update would hang forever resolving the
# mirror hostname. Written here, restored to the networkd stub at the end.
rm -f /mnt/vm/etc/resolv.conf
echo "nameserver 223.5.5.5" > /mnt/vm/etc/resolv.conf

chroot /mnt/vm /bin/bash -s <<'CHROOT'
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
# same fail-fast apt timeout config inside the guest rootfs
cat > /etc/apt/apt.conf.d/99timeout <<'APTCONF'
Acquire::http::Timeout "30";
Acquire::https::Timeout "30";
Acquire::Retries "3";
APTCONF
apt-get -qq update >/dev/null
# openssh + X deps for headless chromium; socat for the CDP relay below;
# nfs-common for the shared /workspace mount (the NFS client is built into the
# Firecracker kernel; only the mount.nfs helper is missing from the base image).
# git/ripgrep/python3 are for Bash routed into the guest: the agent's commands
# assume a developer image, and a bare Debian makes every routed command fail.
apt-get -qq install -y openssh-server chromium xvfb xdotool fonts-noto-cjk \
    ca-certificates curl less procps iproute2 cloud-guest-utils socat x11vnc \
    nfs-common git ripgrep python3 jq unzip tar gzip \
    >/dev/null
systemctl enable ssh
# NFS exports use all_squash, so every file in /workspace shows up owned by the
# anonymous uid while git runs as root. Without this, every git command inside
# the guest dies with "detected dubious ownership in repository at '/workspace'".
git config --system --add safe.directory /workspace
systemctl enable systemd-networkd

# Boot-time trim: disable services useless for an SSH+browser microVM. Measured
# cold boot was 10.7s userspace; these three alone cost ~2.3s (resolved 1.66s,
# avahi 0.67s) with zero value here. DNS uses a static /etc/resolv.conf (set
# below), so disabling resolved does NOT break name resolution. timesyncd is
# KEPT on purpose: a wrong clock breaks TLS cert validation in the browser.
systemctl disable systemd-resolved.service 2>/dev/null || true
systemctl disable avahi-daemon.service avahi-daemon.socket 2>/dev/null || true
systemctl disable unattended-upgrades.service 2>/dev/null || true
systemctl disable e2scrub_reap.service 2>/dev/null || true

# Headless CDP chromium, split across three units. Two non-obvious reasons:
#   1. Xvfb must be its OWN service, not a chromium ExecStartPre. Xvfb runs in
#      the foreground and never daemonizes, so an ExecStartPre never returns —
#      chromium's ExecStart would never fire and nothing listens on 9222.
#   2. Chrome binds the /json HTTP endpoint to 127.0.0.1 only, IGNORING
#      --remote-debugging-address=0.0.0.0 (deliberate Chrome security). The host
#      CDP client reaches the guest over the tap net at <guest-ip>:9222, so a
#      socat relay re-exposes loopback 9221 on 0.0.0.0:9222.
cat > /etc/systemd/system/xvfb.service <<UNIT
[Unit]
Description=Xvfb virtual framebuffer on :99
Before=chromium-cdp.service

[Service]
ExecStart=/usr/bin/Xvfb :99 -screen 0 1920x1080x24 -nolisten tcp
Restart=always

[Install]
WantedBy=multi-user.target
UNIT

cat > /etc/systemd/system/chromium-cdp.service <<UNIT
[Unit]
Description=Chromium (CDP on :9222)
After=network-online.target xvfb.service
Requires=xvfb.service

[Service]
Environment=DISPLAY=:99
# NOT --headless: chromium renders into the Xvfb :99 framebuffer so x11vnc can
# mirror the SAME screen the agent drives via CDP. A human VNC viewer and the
# agent thus share one browser window + profile (hand-off: log in by hand, let
# the agent continue with the live session). CDP works identically headful.
ExecStart=/usr/bin/chromium --no-sandbox --disable-gpu \
    --remote-debugging-port=9221 --start-maximized \
    --user-data-dir=/root/.chromium-cdp --no-first-run --disable-dev-shm-usage
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
UNIT

cat > /etc/systemd/system/cdp-relay.service <<UNIT
[Unit]
Description=CDP relay 0.0.0.0:9222 -> 127.0.0.1:9221
After=chromium-cdp.service
Requires=chromium-cdp.service

[Service]
ExecStart=/usr/bin/socat TCP-LISTEN:9222,fork,reuseaddr,bind=0.0.0.0 TCP:127.0.0.1:9221
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
UNIT

# Human take-over viewer: x11vnc mirrors the SAME Xvfb :99 screen the headful
# chromium renders into, exposed on 0.0.0.0:5900. A human connects with any VNC
# client to watch/drive the live browser (e.g. log in by hand), then hands the
# session back to the agent — both share one browser window + profile.
# -nopw: no VNC auth. The port is only reachable over the private VM network
# (Firecracker: guest IP on the host tap; QEMU: 127.0.0.1 hostfwd), never the
# public internet. -forever/-shared: survive client disconnects + allow the
# agent's CDP and a human viewer to be attached at the same time.
cat > /etc/systemd/system/x11vnc.service <<UNIT
[Unit]
Description=x11vnc (human viewer on :5900, mirrors Xvfb :99)
After=xvfb.service
Requires=xvfb.service

[Service]
Environment=DISPLAY=:99
ExecStart=/usr/bin/x11vnc -display :99 -forever -shared -nopw -rfbport 5900 -listen 0.0.0.0 -noxdamage -quiet
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
UNIT

ln -sf /etc/systemd/system/xvfb.service /etc/systemd/system/multi-user.target.wants/xvfb.service
ln -sf /etc/systemd/system/chromium-cdp.service /etc/systemd/system/multi-user.target.wants/chromium-cdp.service
ln -sf /etc/systemd/system/cdp-relay.service /etc/systemd/system/multi-user.target.wants/cdp-relay.service
ln -sf /etc/systemd/system/x11vnc.service /etc/systemd/system/multi-user.target.wants/x11vnc.service

# Grow the filesystem on first boot (instance disks are truncated larger than
# the base image). The root disk is a BARE ext4 (no partition table), so this
# is a plain `resize2fs /dev/vda` — NOT growpart + resize2fs /dev/vda1.
cat > /etc/systemd/system/firstboot-growfs.service <<UNIT2
[Unit]
Description=Grow root filesystem on first boot
ConditionPathExists=!/var/lib/growfs-done
DefaultDependencies=no
After=systemd-udevd.service local-fs.target
Before=multi-user.target

[Service]
Type=oneshot
ExecStart=/bin/sh -c "resize2fs /dev/vda; touch /var/lib/growfs-done"
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
UNIT2
ln -sf /etc/systemd/system/firstboot-growfs.service /etc/systemd/system/multi-user.target.wants/firstboot-growfs.service

# Root is a bare ext4 mounted as /dev/vda. The stock cloud image's fstab
# references the root (and ESP) by PARTUUID — on a bare fs those devices never
# appear, so systemd blocks ~90s waiting for them before failing to multi-user.
# Overwrite with a single /dev/vda root entry.
cat > /etc/fstab <<'FSTAB'
/dev/vda / ext4 rw,discard,errors=remount-ro 0 1
FSTAB

# Shared host project dir. The host exports <project> to this VM's tap /30 and
# passes the full `<server>:<path>` spec on the kernel command line
# (nonoclaw.nfsserver=...), because NFSv4 mounts the export's own root — a bare
# server would land the guest on `/` instead of the project.
# nfs4 only: the v3 client in this kernel rejects the options mount.nfs passes.
# bg/soft so an unreachable export never blocks boot.
mkdir -p /workspace
cat > /usr/local/sbin/nonoclaw-workspace-mount <<'HELPER'
#!/bin/sh
spec="$(sed -n 's/.*nonoclaw\.nfsserver=\([^ ]*\).*/\1/p' /proc/cmdline)"
[ -n "$spec" ] || exit 0
mountpoint -q /workspace && exit 0
exec mount -t nfs4 -o "bg,soft,timeo=50,retrans=2" "$spec" /workspace
HELPER
chmod +x /usr/local/sbin/nonoclaw-workspace-mount
cat > /etc/systemd/system/workspace-mount.service <<'UNIT'
[Unit]
Description=Mount the host project directory at /workspace
After=network-online.target
Wants=network-online.target
ConditionPathIsDirectory=/workspace
ConditionKernelCommandLine=nonoclaw.nfsserver

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/sbin/nonoclaw-workspace-mount
ExecStop=/bin/umount -l /workspace

[Install]
WantedBy=multi-user.target
UNIT
systemctl enable workspace-mount.service

# Proxy for the guest, read from the same kernel cmdline (nonoclaw.proxy=host:port).
# Exported via /etc/profile.d so interactive shells and apt/curl/pip pick it up.
# Absent parameter -> no proxy, and the guest uses direct NAT.
cat > /usr/local/sbin/nonoclaw-guest-proxy <<'HELPER'
#!/bin/sh
# Emit proxy exports for the guest, read from the kernel cmdline. Prints nothing
# when no proxy was configured, so the guest falls back to direct NAT.
proxy="$(sed -n 's/.*nonoclaw\.proxy=\([^ ]*\).*/\1/p' /proc/cmdline)"
[ -n "$proxy" ] || exit 0
echo "export http_proxy=http://$proxy"
echo "export https_proxy=http://$proxy"
echo "export HTTP_PROXY=http://$proxy"
echo "export HTTPS_PROXY=http://$proxy"
echo "export no_proxy=localhost,127.0.0.1,172.29.0.0/16"
echo "export NO_PROXY=localhost,127.0.0.1,172.29.0.0/16"
HELPER
chmod +x /usr/local/sbin/nonoclaw-guest-proxy
cat > /etc/profile.d/00-nonoclaw-proxy.sh <<'PROFILE'
[ -x /usr/local/sbin/nonoclaw-guest-proxy ] && \
  eval "$(/usr/local/sbin/nonoclaw-guest-proxy)"
PROFILE

# DHCP client for networkd
apt-get -qq install -y systemd-resolved >/dev/null 2>&1 || true
echo "nameserver 223.5.5.5" > /etc/resolv.conf
chroot_rc=0
CHROOT
chroot_rc=$?

# 6. cleanup unmounts
# The chroot bind-mounted proc/sys/dev recursively (--rbind), so detach them
# recursively first. --rbind /dev pulls in nested submounts (devpts, mqueue,
# shm) that keep /mnt/vm busy; -R handles the whole subtree.
umount -R /mnt/vm/proc 2>/dev/null || true
umount -R /mnt/vm/sys 2>/dev/null || true
umount -R /mnt/vm/dev 2>/dev/null || true
[ "$chroot_rc" -eq 0 ] || exit "$chroot_rc"

# --- kernel + initrd ---------------------------------------------------------
# (rootfs stays mounted; chroot only unmounted proc/sys/dev above)
mkdir -p /assets
# Kernel: use the Firecracker CI ELF vmlinux bind-mounted at /fc-kernel.bin, NOT
# the Debian /boot/vmlinuz-* (which is an LZMA bzImage that Firecracker's loader
# rejects with InvalidElfMagicNumber). The matching Debian initrd is still used.
cp /fc-kernel.bin /assets/kernel.img
KVER=$(ls /mnt/vm/boot | grep -E '^vmlinuz-[0-9]' | sed 's/^vmlinuz-//' | sort -V | tail -1)
cp "/mnt/vm/boot/initrd.img-$KVER" /assets/initrd.img
# QEMU kernel: the Debian distro bzImage (full virtio-pci + PCI + ACPI). The FC
# vmlinux has none of those — it boots under Firecracker (virtio-mmio, auto-
# injected cmdline) but under QEMU q35 never finds /dev/vda and under microvm
# freezes at ACPI init. QEMU's -kernel loader accepts the LZMA bzImage that
# Firecracker's loader rejects, so macOS/QEMU boots this one instead.
cp "/mnt/vm/boot/vmlinuz-$KVER" /assets/kernel-qemu.img
echo "kernel: FC vmlinux (ELF) + Debian bzImage (QEMU); initrd: $KVER"
# Sync the loop-mounted rootfs, then detach it. A plain `umount` here aborts the
# build under `set -e` if the mount is briefly busy; lazy-unmount (-l) always
# succeeds and the preceding sync guarantees data is flushed to the raw image.
sync
umount -l /mnt/vm 2>/dev/null || true
# brief settle so the loop device releases before the final copy reads the raw
sleep 1

# --- final image: BARE ext4 ---------------------------------------------------
# The source is an MBR partition disk (root ext4 in p1 at byte offset
# P1_OFFSET). Firecracker's minimal CI kernel does NOT scan the virtio-blk MBR,
# so a /dev/vda1 node never appears — the guest must boot root=/dev/vda off a
# BARE ext4 filesystem. Extract exactly the p1 region (skip the partition table
# + any ESP) as a standalone ext4 image.
#
# dd (not cp): we need to start reading at P1_OFFSET. GNU dd's conv=sparse keeps
# the holes so the output stays small (busybox dd lacks conv=sparse — this runs
# in debian:12 so GNU dd is available). bs=1M with iflag=skip_bytes lets us skip
# a non-block-aligned-looking offset cleanly.
echo "extracting bare ext4 -> /assets/rootfs-bare.ext4 (p1 @ $P1_OFFSET)"
dd if=/work/rootfs.raw of=/assets/rootfs-bare.ext4 bs=1M \
   iflag=skip_bytes skip=$P1_OFFSET conv=sparse status=none
# Validate the extracted filesystem before declaring success.
e2fsck -fn /assets/rootfs-bare.ext4 >/dev/null || { echo "e2fsck failed on bare ext4"; exit 1; }
rm -f /work/rootfs.raw
echo "===BUILD_OK==="
EOS

[ -f "$ASSETS_DIR/kernel.img" ] && [ -f "$ASSETS_DIR/initrd.img" ] \
  && [ -f "$ASSETS_DIR/rootfs-bare.ext4" ] \
  || die "assets missing after build"

log "done: $ASSETS_DIR/rootfs-bare.ext4 ($(du -h "$ASSETS_DIR/rootfs-bare.ext4" | cut -f1)), kernel.img, initrd.img"
