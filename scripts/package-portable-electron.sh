#!/usr/bin/env bash
# Assemble the Windows portable (green / zero-install) Electron build.
#
# Target scenario: domain-controlled machines where no software may be
# installed — the package must carry every runtime nonoclaw touches:
#   - Electron shell + SPA (electron-builder --win --dir → win-unpacked/)
#   - nonoclaw.exe backend (mingw cross build)
#   - node + npm        → MCP stdio servers (pre-installed, offline)
#   - python embeddable → markitdown (pre-installed via wine, offline)
#   - rg.exe            → Grep tool
#   - MinGit            → git snapshot + agent git commands
#   - poppler           → pdftotext PDF fallback
#
# Usage:
#   scripts/package-portable-electron.sh            # assemble from cache
#   scripts/package-portable-electron.sh --skip-rust  # reuse staged exe
# Artifacts cached in release/runtime-cache/ (re-runs are incremental).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FRONTEND="$ROOT/frontend"
REL="$ROOT/release"
CACHE="$REL/runtime-cache"
OUT_DIR="$REL"
SKIP_RUST=0
[ "${1:-}" = "--skip-rust" ] && SKIP_RUST=1

PROXY="${PROXY:-http://127.0.0.1:20171}"
export WINEPREFIX="${WINEPREFIX:-$HOME/.wine-nonoclaw}"

NODE_VERSION=22.14.0
PY_VERSION=3.12.8
RG_VERSION=14.1.1
MINGIT_VERSION=2.47.1
POPPLER_VERSION=24.08.0
# MCP servers pre-installed for offline use. Keys = names in the settings
# template; values = "package[@version]" spec for npm.
MCP_PKGS=(
  "context7:@upstash/context7-mcp"
  "github:@modelcontextprotocol/server-github"
)

log() { echo "=== $* ==="; }

# ---------------------------------------------------------------------------
log "[0/8] Preflight"
# ---------------------------------------------------------------------------
command -v x86_64-w64-mingw32-gcc >/dev/null || { echo "mingw not installed"; exit 1; }
command -v wine >/dev/null || { echo "wine not installed"; exit 1; }
mkdir -p "$CACHE"

fetch() { # fetch <url> <dest-file>
  local dest="$CACHE/$2"
  [ -s "$dest" ] && { echo "  cached: $2"; return 0; }
  echo "  downloading: $2"
  curl -sL --proxy "$PROXY" --retry 3 -o "$dest" "$1"
}

# ---------------------------------------------------------------------------
if [ "$SKIP_RUST" -eq 0 ]; then
  log "[1/8] Cross-build nonoclaw.exe (mingw)"
  (cd "$ROOT/rust" && cargo build --release -p nonoclaw --target x86_64-pc-windows-gnu)
fi
WIN_EXE="$ROOT/rust/target/x86_64-pc-windows-gnu/release/nonoclaw.exe"
[ -f "$WIN_EXE" ] || { echo "missing $WIN_EXE"; exit 1; }

# ---------------------------------------------------------------------------
log "[2/8] Electron builder: win-unpacked"
# ---------------------------------------------------------------------------
cd "$FRONTEND"
# Stage ONLY the .exe — the extraResources filter matches both names.
rm -f desktop/bin/nonoclaw
install -m 0755 "$WIN_EXE" desktop/bin/nonoclaw.exe
npm run build
# --dir only: NSIS installer is pointless for a green build.
npx electron-builder --win --dir --x64
UNPACKED="$FRONTEND/desktop/release/win-unpacked"
[ -f "$UNPACKED/NonoClaw.exe" ] || { echo "electron-builder produced no win-unpacked/NonoClaw.exe"; exit 1; }

# ---------------------------------------------------------------------------
log "[3/8] Fetch runtime distributions"
# ---------------------------------------------------------------------------
fetch "https://nodejs.org/dist/v$NODE_VERSION/node-v${NODE_VERSION}-win-x64.zip" "node-v${NODE_VERSION}-win-x64.zip"
fetch "https://www.python.org/ftp/python/$PY_VERSION/python-$PY_VERSION-embed-amd64.zip" "python-$PY_VERSION-embed-amd64.zip"
fetch "https://bootstrap.pypa.io/get-pip.py" "get-pip.py"
fetch "https://github.com/BurntSushi/ripgrep/releases/download/$RG_VERSION/ripgrep-$RG_VERSION-x86_64-pc-windows-msvc.zip" "ripgrep-$RG_VERSION-win.zip"
fetch "https://github.com/git-for-windows/git/releases/download/v$MINGIT_VERSION.windows.1/MinGit-$MINGIT_VERSION-64-bit.zip" "mingit-$MINGIT_VERSION-64-bit.zip"
fetch "https://github.com/oschwartz10612/poppler-windows/releases/download/v$POPPLER_VERSION-0/Release-$POPPLER_VERSION-0.zip" "poppler-$POPPLER_VERSION-win.zip"
for f in "$CACHE"/*.zip; do unzip -tqq "$f" >/dev/null 2>&1 || { echo "corrupt zip: $f"; exit 1; }; done

# ---------------------------------------------------------------------------
log "[4/8] Assemble runtime/ tree"
# ---------------------------------------------------------------------------
PKG="$OUT_DIR/NonoClawPortable-electron"
rm -rf "$PKG"
mkdir -p "$PKG/runtime/bin" "$PKG/templates"
cp -a "$UNPACKED/." "$PKG/"

RT="$PKG/runtime"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# node (full zip: npm + npx-cli.js needed for MCP management)
unzip -qq "$CACHE/node-v${NODE_VERSION}-win-x64.zip" -d "$tmp"
mv "$tmp/node-v${NODE_VERSION}-win-x64" "$RT/node"
# python embeddable
unzip -qq "$CACHE/python-$PY_VERSION-embed-amd64.zip" -d "$RT/python"
# rg
unzip -qq "$CACHE/ripgrep-$RG_VERSION-win.zip" -d "$tmp"
mv "$tmp/ripgrep-$RG_VERSION-x86_64-pc-windows-msvc/rg.exe" "$RT/bin/rg.exe"
# MinGit (user decision: include — git snapshot + agent git need it)
unzip -qq "$CACHE/mingit-$MINGIT_VERSION-64-bit.zip" -d "$RT/git"
# poppler (user decision: include — pdftotext PDF fallback)
unzip -qq "$CACHE/poppler-$POPPLER_VERSION-win.zip" -d "$tmp"
mv "$tmp/poppler-$POPPLER_VERSION" "$RT/poppler"

# MSVC runtime (2022 redist, plain DLLs extracted via cabextract). App-local
# deployment is Microsoft-sanctioned — no admin/vc_redist install needed on
# the locked-down target machine.
VC_CACHE="$CACHE/vc-runtime"
mkdir -p "$VC_CACHE"
if [ ! -f "$VC_CACHE/vcruntime140.dll" ]; then
  fetch "https://aka.ms/vs/17/release/vc_redist.x64.exe" vc_redist.x64.exe
  vctmp=$(mktemp -d)
  cabextract -q "$CACHE/vc_redist.x64.exe" -d "$vctmp"
  for c in "$vctmp"/a*; do
    file -b "$c" | grep -q "Cabinet" && cabextract -qq "$c" -d "$vctmp/out" 2>/dev/null || true
  done
  install -m 0644 "$vctmp/out/vcruntime140.dll_amd64"        "$VC_CACHE/vcruntime140.dll"
  install -m 0644 "$vctmp/out/vcruntime140_1.dll_amd64"      "$VC_CACHE/vcruntime140_1.dll"
  install -m 0644 "$vctmp/out/vcruntime140_threads.dll_amd64" "$VC_CACHE/vcruntime140_threads.dll"
  install -m 0644 "$vctmp/out/msvcp140.dll_amd64"            "$VC_CACHE/msvcp140.dll"
  install -m 0644 "$vctmp/out/msvcp140_1.dll_amd64"          "$VC_CACHE/msvcp140_1.dll"
  install -m 0644 "$vctmp/out/msvcp140_2.dll_amd64"          "$VC_CACHE/msvcp140_2.dll"
  install -m 0644 "$vctmp/out/concrt140.dll_amd64"           "$VC_CACHE/concrt140.dll"
  rm -rf "$vctmp"
fi
mkdir -p "$RT/vcrt"
cp "$VC_CACHE"/*.dll "$RT/vcrt/"

# ---------------------------------------------------------------------------
log "[5/8] Pre-install markitdown into embeddable python (wine, offline result)"
# ---------------------------------------------------------------------------
PYEXE="$RT/python/python.exe"
# Embeddable python needs importlib pinned to its own DLL dir first.
cat > "$RT/python/python312._pth" <<EOF
python312.zip
.
Lib\site-packages
import site
EOF
mkdir -p "$RT/python/Lib/site-packages"
# wine executes the Windows python to bootstrap pip + markitdown. Everything
# lands inside $RT/python — the target machine never runs an installer.
#
# PROXY WARNING: all_proxy=socks:// breaks pip inside wine (no SOCKS support).
# PROXY NOTE: pip needs HTTP(S) proxy vars in curl-compatible form; strip
# all_proxy (socks) which pip-under-wine cannot handle.
PIP_ENV=(env -u ALL_PROXY -u all_proxy)
if [ -n "${PIP_PROXY:-}" ]; then
  PIP_ENV+=(http_proxy="$PIP_PROXY" https_proxy="$PIP_PROXY")
fi
# numpy MUST be pinned <2: numpy 2.x imports crealf from api-ms-win-crt-math,
# which wine 9.0's builtin ucrtbase does not export (verified 2026-09-02:
# numpy 2.5.2 crashes on import under wine, 1.26.4 works; markitdown itself
# runs fine on 1.26.4). On the real Windows target numpy 2.x would also work,
# but one pinned version keeps build-machine verification == shipped bits.
"${PIP_ENV[@]}" wine "$PYEXE" "$CACHE/get-pip.py" --no-warn-script-location -q
"${PIP_ENV[@]}" wine "$PYEXE" -m pip install "numpy==1.26.4" --no-warn-script-location -q
"${PIP_ENV[@]}" wine "$PYEXE" -m pip install "markitdown[pdf,docx,pptx,xlsx]" --no-warn-script-location -q
# markitdown's CLI entry point is a pip-generated .exe — replace with a
# self-locating cmd shim that needs no Scripts/ dir on PATH ordering.
cat > "$RT/bin/markitdown.cmd" <<EOF
@echo off
"%~dp0..\python\python.exe" -m markitdown %*
EOF

# ---------------------------------------------------------------------------
log "[6/8] Pre-install MCP servers (offline node_modules)"
# ---------------------------------------------------------------------------
mkdir -p "$RT/node-mcp"
cd "$RT/node-mcp"
for entry in "${MCP_PKGS[@]}"; do
  name="${entry%%:*}"; spec="${entry#*:}"
  npm install --omit=dev --no-audit --no-fund --prefix "$RT/node-mcp/$name" "$spec"
done
# Windows npx is npx.cmd — BatBadBut hardening makes Rust/tokio refuse to
# spawn .cmd directly, and the domain machine is offline anyway. Ship direct
# node-invokable entry shims per server instead. Shims use __dirname so they
# work wherever the package is unpacked (no absolute build-time paths).
MCPROOT="$RT/node-mcp" node <<'EOF'
const fs = require("fs"), path = require("path");
const root = process.env.MCPROOT;
for (const name of fs.readdirSync(root)) {
  const nm = path.join(root, name, "node_modules");
  if (!fs.existsSync(nm)) continue;
  for (const entryName of fs.readdirSync(nm)) {
    const rels = entryName.startsWith("@")
      ? fs.readdirSync(path.join(nm, entryName)).map((p) => path.join(entryName, p))
      : [entryName];
    for (const rel of rels) {
      const pkgJsonPath = path.join(nm, rel, "package.json");
      if (!fs.existsSync(pkgJsonPath)) continue;
      const pkg = JSON.parse(fs.readFileSync(pkgJsonPath, "utf8"));
      if (!pkg.bin) continue;
      const binRel =
        typeof pkg.bin === "string" ? pkg.bin : pkg.bin[Object.keys(pkg.bin)[0]];
      const target = path.join(nm, rel, binRel);
      fs.writeFileSync(
        path.join(root, name + ".js"),
        `require(${JSON.stringify(target)});\n`
      );
      console.log(`  shim: ${name}.js -> ${rel}/${binRel}`);
    }
  }
}
EOF
for entry in "${MCP_PKGS[@]}"; do
  [ -f "$RT/node-mcp/${entry%%:*}.js" ] || { echo "shim missing for ${entry%%:*}"; exit 1; }
done
# zhihuiya remote-MCP stdio bridge (ESM, has own package.json deps) — source
# of truth lives in scripts/mcp-proxies/ (version-controlled); it needs
# runtime\node on PATH via the `node.exe` command in the template.
ZHY_SRC="$ROOT/scripts/mcp-proxies"
if [ -f "$ZHY_SRC/zhihuiya-proxy.mjs" ]; then
  mkdir -p "$RT/node-mcp/zhihuiya"
  cp "$ZHY_SRC/zhihuiya-proxy.mjs" "$RT/node-mcp/zhihuiya/"
  cp "$ZHY_SRC/package.json" "$ZHY_SRC/package-lock.json" "$RT/node-mcp/zhihuiya/" 2>/dev/null || true
  npm install --omit=dev --no-audit --no-fund --prefix "$RT/node-mcp/zhihuiya"
fi

# ---------------------------------------------------------------------------
log "[7/8] Settings template + docs"
# ---------------------------------------------------------------------------
cat > "$PKG/templates/settings.json" <<'EOF'
{
  "promptProfile": "full",
  "autoCompact": true,
  "permissions": { "defaultMode": "auto", "allow": [] },
  "autoSelectMcp": true,
  "autoSelectMcpTopK": 15,
  "attachmentConverter": "auto",
  "mcpServers": {
    "context7": {
      "command": "node.exe",
      "args": ["${NONOCLAW_PORTABLE_ROOT}\\runtime\\node-mcp\\context7.js"]
    },
    "github": {
      "command": "node.exe",
      "args": ["${NONOCLAW_PORTABLE_ROOT}\\runtime\\node-mcp\\github.js"]
    },
    "zhihuiya": {
      "command": "node.exe",
      "args": ["${NONOCLAW_PORTABLE_ROOT}\\runtime\\node-mcp\\zhihuiya\\zhihuiya-proxy.mjs"],
      "env": { "ZHIHUIYA_API_KEY": "在此填入智汇芽 API Key" }
    }
  }
}
EOF
# Template uses ${NONOCLAW_PORTABLE_ROOT} (set by main.cjs to the package
# root) rather than ${NONOCLAW_HOME}\.. — the latter breaks when the
# read-only fallback moves NONOCLAW_HOME to %USERPROFILE%\.nonoclaw.
# main.cjs must export NONOCLAW_PORTABLE_ROOT in portableBackendEnv; the
# placeholder expander in mcp.rs only knows NONOCLAW_HOME/HOME/USERPROFILE,
# so main.cjs resolves it by pre-expanding the template at seed time.

cat > "$PKG/启动说明.txt" <<'EOF'
NonoClaw 绿色版（免安装）
==========================

启动：双击 NonoClaw.exe。首次启动自动初始化。

数据存放（优先级）：
  1. 本目录下 .nonoclaw-home\     —— 目录可写时使用（推荐：整个目录可拷贝到 U 盘随身携带）
  2. %USERPROFILE%\.nonoclaw\     —— 本目录只读（如 Program Files）时自动回退

内置运行时（无需安装任何组件）：
  runtime\node\      Node.js（MCP 服务器已离线预装）
  runtime\python\    Python + MarkItDown（文档解析，离线预装）
  runtime\bin\       rg / markitdown
  runtime\git\       MinGit
  runtime\poppler\   PDF 工具

配置：首次启动会生成本目录 .nonoclaw-home\settings.json，
      需要填写 LLM API key 后才能对话。
EOF

# ---------------------------------------------------------------------------
log "[8/8] Validate + zip"
# ---------------------------------------------------------------------------
# md5 guard: staged backend binary must match the cross-build output.
[ "$(md5sum < "$PKG/resources/nonoclaw/bin/nonoclaw.exe" | cut -d' ' -f1)" = \
  "$(md5sum < "$WIN_EXE" | cut -d' ' -f1)" ] || { echo "backend binary drift!"; exit 1; }
for f in "$RT/node/node.exe" "$RT/python/python.exe" "$RT/bin/rg.exe" \
         "$RT/bin/markitdown.cmd" "$RT/git/cmd/git.exe" \
         "$RT/poppler/Library/bin/pdftotext.exe" \
         "$RT/vcrt/vcruntime140.dll" "$RT/vcrt/msvcp140.dll" \
         "$RT/node-mcp/context7.js" "$PKG/resources/app.asar" \
         "$PKG/templates/settings.json"; do
  [ -f "$f" ] || { echo "MISSING: $f"; exit 1; }
done
# numpy pin guard — numpy 2.x breaks under wine ucrtbase (see step 5 note)
[ -d "$RT/python/Lib/site-packages/numpy" ] || { echo "MISSING: numpy"; exit 1; }
NP_VER=$(ls "$RT/python/Lib/site-packages/" | grep -oP '^numpy-\K[0-9.]+' | head -1)
case "$NP_VER" in 1.*) ;; *) echo "numpy version $NP_VER is not 1.x — refuses wine verification"; exit 1;; esac
# smoke: markitdown converts an HTML sample via wine (build-machine proof)
printf '<html><body><h1>pkgtest</h1></body></html>' > "$tmp/t.html"
"${PIP_ENV[@]}" WINEPREFIX="$WINEPREFIX" timeout 180 wine "$PYEXE" -m markitdown "$(cygpath -w "$tmp/t.html" 2>/dev/null || echo "$tmp/t.html")" 2>/dev/null | grep -q "pkgtest" \
  || { echo "markitdown smoke test failed"; exit 1; }

VERSION=$(node -e "console.log(require('$FRONTEND/package.json').version)")
ZIP="$OUT_DIR/nonoclaw-desktop-v$VERSION-windows-electron-portable.zip"
rm -f "$ZIP"
(cd "$OUT_DIR" && zip -rq "$ZIP" NonoClawPortable-electron \
  -x "*/node-mcp/*/node_modules/.cache/*" -x "*.DS_Store")

echo
echo "✓ Portable package: $ZIP"
ls -lh "$ZIP"
echo "  unpacked size: $(du -sh "$PKG" | cut -f1)"
