# macOS 12 (Monterey / Intel) QEMU via UTM dylibs

Homebrew dropped Monterey (Tier 3) bottles, so `brew install qemu` on macOS 12
wants to compile qemu plus ~37 dependencies from source. UTM 4.6.x — the last
release line supporting macOS 12 — bundles QEMU 9.1.2 (with `hvf` and slirp) as
dylib frameworks. This launcher dlopens those dylibs so the usual
`qemu-system-x86_64` / `qemu-img` commands appear on PATH, fully unprivileged.

## Install (no sudo; /usr/local/bin is admin-writable on stock macOS)

```sh
# 1. download UTM 4.6.3 and copy the qemu payload out
curl -L -o /tmp/UTM.dmg https://github.com/utmapp/UTM/releases/download/v4.6.3/UTM.dmg
hdiutil attach /tmp/UTM.dmg -nobrowse -readonly
mkdir -p /usr/local/lib/qemu-utm
rsync -a /Volumes/UTM/UTM.app/Contents/Frameworks/ /usr/local/lib/qemu-utm/Frameworks/
rsync -a /Volumes/UTM/UTM.app/Contents/Resources/qemu/  /usr/local/lib/qemu-utm/share/
hdiutil detach /Volumes/UTM && rm /tmp/UTM.dmg   # ~1.1 GB on disk

# 2. build + install the launchers (one binary, dispatched by argv[0])
cd scripts/vm/macos-qemu-launcher
clang -arch x86_64 -O2 -rpath /usr/local/lib/qemu-utm/Frameworks \
      -o /tmp/qemu-launcher qemu_launcher.c
cp /tmp/qemu-launcher /usr/local/bin/qemu-system-x86_64
cp /tmp/qemu-launcher /usr/local/bin/qemu-img

# 3. verify
qemu-system-x86_64 --version      # QEMU emulator version 9.1.2
qemu-system-x86_64 -accel help    # tcg, hvf
```

NonoClaw's QEMU backend looks up `qemu-system-x86_64` on PATH, so nothing else
is needed at runtime.

Notes:

- The softmmu dylib exports the modular API (`qemu_init`/`qemu_main_loop`/
  `qemu_cleanup`) rather than `main()`; the launcher also injects `-L <share>`
  so SeaBIOS/pxe firmware files resolve.
- `qemu-img` dylib exports `main()`; argv is forwarded verbatim.
- The license for the extracted QEMU binaries is GPL-2; UTM's redistribution
  terms apply. This is for local use.
- If a future UTM release updates the bundled QEMU, re-run step 1 only.
