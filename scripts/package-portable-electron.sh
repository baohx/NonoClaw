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

# Direct internet access works on this machine; the loopback v2rayA proxy is
# only required on machines without direct egress. Probe and fall back.
probe() { curl -s --max-time 6 -x "$1" https://pypi.org/simple/ -o /dev/null; }
if ! probe "http://127.0.0.1:20171"; then
  # HEAD-range probe: /simple/ is a 45 MB index; -o /dev/null on a slow link
  # times out mid-body even though egress works.
  if curl -s --max-time 15 -r 0-0 -o /dev/null https://pypi.org/simple/pip/; then
    PROXY=""
    echo "  proxy 127.0.0.1:20171 unreachable — using direct connection"
  else
    echo "  no network egress available (proxy and direct both failed)"; exit 1
  fi
else
  PROXY="http://127.0.0.1:20171"
fi
PROXY="${PROXY:-${EXTRA_PROXY:-}}"
export WINEPREFIX="${WINEPREFIX:-$HOME/.wine-nonoclaw}"
# Windows python.exe under wine aborts during console init when it inherits
# no controlling terminal ("init_sys_streams ... WinError 6 invalid handle"
# — reproduced 2026-09-09 over non-tty SSH). Run every wine invocation on a
# pseudo-terminal so the child sees a real console.
wine() {
  command script -qec "wine $*" /dev/null </dev/null
}

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
  "playwright:playwright-mcp"
  "lark-cli:@larksuite/cli"
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
# Portable rust toolchain (windows-gnu standalone components; no rustup
# metadata, no admin, self-contained). Keys in toolchain.rs probe names:
# rustc/cargo/rustup. The std+rust-mingw components ride along so `cargo
# build` actually works for x86_64-pc-windows-gnu targets.
RUSTC_VERSION=1.98.1
RUST_DIST_DATE=2026-09-03
fetch "https://static.rust-lang.org/dist/$RUST_DIST_DATE/rustc-$RUSTC_VERSION-x86_64-pc-windows-gnu.tar.gz" "rustc-$RUSTC_VERSION-x86_64-pc-windows-gnu.tar.gz"
fetch "https://static.rust-lang.org/dist/$RUST_DIST_DATE/rust-std-$RUSTC_VERSION-x86_64-pc-windows-gnu.tar.gz" "rust-std-$RUSTC_VERSION-x86_64-pc-windows-gnu.tar.gz"
fetch "https://static.rust-lang.org/dist/$RUST_DIST_DATE/cargo-$RUSTC_VERSION-x86_64-pc-windows-gnu.tar.gz" "cargo-$RUSTC_VERSION-x86_64-pc-windows-gnu.tar.gz"
fetch "https://static.rust-lang.org/dist/$RUST_DIST_DATE/rust-mingw-$RUSTC_VERSION-x86_64-pc-windows-gnu.tar.gz" "rust-mingw-$RUSTC_VERSION-x86_64-pc-windows-gnu.tar.gz"
fetch "https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe" "rustup-init.exe"
for f in "$CACHE"/*.zip; do unzip -tqq "$f" >/dev/null 2>&1 || { echo "corrupt zip: $f"; exit 1; }; done
for f in "$CACHE"/*.tar.gz; do tar -tzf "$f" >/dev/null 2>&1 || { echo "corrupt tar.gz: $f"; exit 1; }; done

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
# rust toolchain (standalone windows-gnu components merged into runtime/rust).
# The insight panel lights up rust.rustc/rust.cargo via executables overrides;
# rustup.exe ships so rust.rustup resolves too (works standalone as a
# rustup-init-style proxy binary).
mkdir -p "$RT/rust"
for comp in rustc rust-std cargo rust-mingw; do
  tar -xzf "$CACHE/$comp-$RUSTC_VERSION-x86_64-pc-windows-gnu.tar.gz" -C "$tmp"
  # Standalone dist components name their payload dir after the component
  # AND target in inconsistent ways (rustc/ vs rust-std-x86_64-pc-windows-gnu/)
  # — the components file's first line names it.
  compdir="$tmp/$comp-$RUSTC_VERSION-x86_64-pc-windows-gnu"
  payload=$(head -1 "$compdir/components")
  [ -n "$payload" ] && [ -d "$compdir/$payload" ] || { echo "payload dir not found for $comp"; exit 1; }
  cp -a "$compdir/$payload/." "$RT/rust/"
  rm -rf "$compdir"
done
mkdir -p "$RT/rust/bin"
cp "$CACHE/rustup-init.exe" "$RT/rust/bin/rustup.exe"
# python venv: the embeddable distribution lacks the venv/ensurepip stdlib
# modules (python312.zip ships without them). Copy them from the nuget
# full package so `python -m venv` works (toolchain python_venv probe).
if [ ! -d "$CACHE/python-nuget-tools" ]; then
  fetch "https://www.nuget.org/api/v2/package/python/$PY_VERSION" "python-$PY_VERSION.nupkg.zip"
  pn=$(mktemp -d)
  unzip -qq "$CACHE/python-$PY_VERSION.nupkg.zip" -d "$pn"
  mkdir -p "$CACHE/python-nuget-tools"
  cp -a "$pn/tools/Lib/venv" "$CACHE/python-nuget-tools/venv"
  cp -a "$pn/tools/Lib/ensurepip" "$CACHE/python-nuget-tools/ensurepip"
  rm -rf "$pn"
fi

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
# venv + ensurepip stdlib modules (see fetch stage above). The embeddable
# _pth only puts Lib\site-packages on sys.path — NOT Lib itself — so these
# must live under site-packages to be importable.
cp -a "$CACHE/python-nuget-tools/venv" "$RT/python/Lib/site-packages/venv"
cp -a "$CACHE/python-nuget-tools/ensurepip" "$RT/python/Lib/site-packages/ensurepip"
# wine executes the Windows python to bootstrap pip + markitdown. Everything
# lands inside $RT/python — the target machine never runs an installer.
#
# PROXY WARNING: all_proxy=socks:// breaks pip inside wine (no SOCKS support).
# PROXY NOTE: pip needs HTTP(S) proxy vars in curl-compatible form; strip
# all_proxy (socks) which pip-under-wine cannot handle.
# All wine calls go through a pty-wrapping helper: Windows python.exe
# aborts console init without a tty (init_sys_streams WinError 6), and
# wine resolves localhost to ::1 which the IPv4 loopback proxies never
# answer — the helper strips proxy vars itself.
mkdir -p "$CACHE/bin"
cat > "$CACHE/bin/wine" <<'WINEHELPER'
#!/usr/bin/env python3
import os, pty, sys, select
for v in ["ALL_PROXY", "all_proxy", "http_proxy", "https_proxy",
          "HTTP_PROXY", "HTTPS_PROXY"]:
    os.environ.pop(v, None)
argv = ["wine"] + sys.argv[1:]
pid, fd = pty.fork()
if pid == 0:
    os.execvp("wine", argv)
while True:
    try:
        r, _, _ = select.select([fd], [], [], 1800)
    except OSError:
        break
    if not r:
        break
    try:
        data = os.read(fd, 65536)
    except OSError:
        break
    if not data:
        break
    os.write(2, data)
_, status = os.waitpid(pid, 0)
sys.exit(os.waitstatus_to_exitcode(status))
WINEHELPER
chmod +x "$CACHE/bin/wine"
wine() { "$CACHE/bin/wine" "$@"; }
PIP_ENV=()
# numpy MUST be pinned <2: numpy 2.x imports crealf from api-ms-win-crt-math,
# which wine 9.0's builtin ucrtbase does not export (verified 2026-09-02:
# numpy 2.5.2 crashes on import under wine, 1.26.4 works; markitdown itself
# runs fine on 1.26.4). On the real Windows target numpy 2.x would also work,
# but one pinned version keeps build-machine verification == shipped bits.
# Wine chokes on deep repo paths for script execution (init_sys_streams
# WinError 6 — reproduced 2026-09-09); staging get-pip on the short Z:	mp
# drive mapping runs it reliably. </dev/null guards non-tty SSH stdin.
cp "$CACHE/get-pip.py" /tmp/get-pip.py
wine "$PYEXE" 'Z:\tmp\get-pip.py' --no-warn-script-location -q </dev/null
wine "$PYEXE" -m pip install "numpy==1.26.4" --no-warn-script-location -q </dev/null
wine "$PYEXE" -m pip install \
  numpy==1.26.4 "markitdown[pdf,docx,pptx,xlsx]" code-review-graph paper-search-mcp crawl4ai \
  fastapi 'uvicorn[standard]' httpx loguru python-dotenv tiktoken \
  --no-warn-script-location -q </dev/null
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
# The generator lives in scripts/mcp-proxies/portable-shims.cjs (version
# controlled); the playwright shim patches listen() to loopback-only so the
# Windows Firewall elevation prompt never fires on domain machines.
MCPROOT="$RT/node-mcp" node "$ROOT/scripts/mcp-proxies/portable-shims.cjs"
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
  cp "$HOME/.nonoclaw/mcp-proxies/package.json" "$HOME/.nonoclaw/mcp-proxies/package-lock.json" \
    "$RT/node-mcp/zhihuiya/"
  npm install --omit=dev --no-audit --no-fund --prefix "$RT/node-mcp/zhihuiya"
fi

mkdir -p "$RT/python-mcp" "$RT/kiro-proxy" "$PKG/.nonoclaw" "$PKG/workspace"
cp "$HOME/.nonoclaw/mcp-proxies/crawl4ai-proxy.py" "$RT/python-mcp/"
cp "$HOME/.nonoclaw/mcp-proxies/miaoda-mcp.py" "$RT/python-mcp/"
cp -a "$HOME/.nonoclaw/ontology-mcp" "$RT/python-mcp/ontology"
rm -rf "$RT/python-mcp/ontology/__pycache__"
cp -a "$HOME/.nonoclaw/plugins/drawio-scientific-illustrator" "$RT/node-mcp/drawio"
rm -rf "$RT/node-mcp/drawio/.git"

KIRO_SRC="${KIRO_PROXY_SOURCE:-$HOME/kiro-gateway}"
[ -f "$KIRO_SRC/main.py" ] || { echo "kiro-proxy source missing: $KIRO_SRC"; exit 1; }
cp "$KIRO_SRC/main.py" "$RT/kiro-proxy/"
cp -a "$KIRO_SRC/kiro" "$RT/kiro-proxy/"
cp "$KIRO_SRC/credentials.json" "$RT/kiro-proxy/"
[ ! -f "$KIRO_SRC/state.json" ] || cp "$KIRO_SRC/state.json" "$RT/kiro-proxy/"
# Keep the current proxy API key but remove machine-local Linux paths/proxies.
if [ -f "$KIRO_SRC/.env" ]; then
  grep -E '^(PROXY_API_KEY|DEBUG_MODE)=' "$KIRO_SRC/.env" > "$RT/kiro-proxy/.env" || true
fi
printf 'KIRO_CREDS_FILE=credentials.json\n' >> "$RT/kiro-proxy/.env"
rm -rf "$RT/kiro-proxy/kiro/__pycache__"

# Copy the current configuration, including API keys, and rewrite only paths.
cp "$HOME/.nonoclaw/settings.json" "$PKG/templates/settings.json"
SETTINGS="$PKG/templates/settings.json" python3 <<'PYSETTINGS'
import json, os
from pathlib import Path
p = Path(os.environ["SETTINGS"])
d = json.loads(p.read_text())
# Resolve "$ENV_VAR" apiKey references against the build machine's env/env
# block so the portable copy never depends on Unix shell variables. Explicit
# env-block values win over process env.
env_block = d.get("env") or {}
import os
def bake(v):
    if isinstance(v, str) and v.startswith("$") and v[1:] in env_block:
        return env_block[v[1:]]
    if isinstance(v, str) and v.startswith("$"):
        return os.environ.get(v[1:], v)
    return v
for m in d.get("models", []):
    if isinstance(m, dict) and "apiKey" in m:
        m["apiKey"] = bake(m["apiKey"])
d["env"] = {k: v for k, v in env_block.items()}
# The current Linux-only v2rayA loopback proxy is not part of this package;
# retaining it would make every provider request fail on Windows.
d.pop("proxy", None)
m = d.setdefault("mcpServers", {})
# ${NONOCLAW_HOME}/.. instead of the package root: NONOCLAW_HOME is resolved
# by the engine at request time from the live process env (main.cjs points it
# at <pkg>/.nonoclaw-home), so the seeded settings.json stays valid when the
# whole package moves between drives (C:, D:, USB). mcp.rs and toolchain.rs
# both expand the placeholder.
r = "${NONOCLAW_HOME}\\.."
def srv(command, rel, env=None, tail=()):
    out = {"command": command, "args": [r + "\\" + rel, *tail]}
    if env: out["env"] = env
    return out
m["context7"] = srv("node.exe", r"runtime\node-mcp\context7.js")
m["github"] = srv("node.exe", r"runtime\node-mcp\github.js")
m["playwright"] = srv("node.exe", r"runtime\node-mcp\playwright.js")
m["zhihuiya"] = srv("node.exe", r"runtime\node-mcp\zhihuiya\zhihuiya-proxy.mjs")
m["drawio-live"] = srv("node.exe", r"runtime\node-mcp\drawio\plugins\drawio-scientific-illustrator\scripts\live-server.mjs")
m["drawio-file-utils"] = srv("node.exe", r"runtime\node-mcp\drawio\plugins\drawio-scientific-illustrator\scripts\server.mjs")
m["code-review-graph"] = {"command":"python.exe", "args":["-m", "code_review_graph", "serve"]}
m["crawl4ai"] = srv("python.exe", r"runtime\python-mcp\crawl4ai-proxy.py")
m["paper-search-mcp"] = {"command":"python.exe", "args":["-m", "paper_search_mcp.server"]}
m["miaoda"] = srv("python.exe", r"runtime\python-mcp\miaoda-mcp.py", {"LARK_CLI": r + r"\runtime\node-mcp\lark-cli.js"})
m["ontology"] = srv("python.exe", r"runtime\python-mcp\ontology\server.py")

# Explicit runtime executable paths so the insight system probe resolves
# every bundled tool from the package regardless of machine PATH order,
# and python.pip resolves to the Scripts/ launcher that ships beside the
# embeddable interpreter. Relative paths expand against the backend cwd
# (PORTABLE_ROOT/workspace), so use the placeholder form instead.
d["executables"] = {
    "node": {"node": {"path": r + r"\runtime\node\node.exe"}},
    "python": {"python": {"path": r + r"\runtime\python\python.exe"}},
    # rust trio: standalone windows-gnu toolchain under runtime/rust lights
    # up rust.rustc/rust.cargo; rustup.exe rides along for rust.rustup.
    "rust": {
        "rustc": {"path": r + r"\runtime\rust\bin\rustc.exe"},
        "cargo": {"path": r + r"\runtime\rust\bin\cargo.exe"},
        "rustup": {"path": r + r"\runtime\rust\bin\rustup.exe"},
    },
}
p.write_text(json.dumps(d, ensure_ascii=False, indent=2) + "\n")
PYSETTINGS

# Dereference Linux symlinks so every skill is physically inside the package.
mkdir -p "$PKG/.nonoclaw/skills"
rsync -aL --delete --exclude '.git' --exclude '__pycache__' --exclude 'node_modules' \
  "$HOME/.nonoclaw/skills/" "$PKG/.nonoclaw/skills/"

# ---------------------------------------------------------------------------
log "[7/8] Settings + docs"
# ---------------------------------------------------------------------------
# The template keeps ${NONOCLAW_HOME} placeholders verbatim; the backend
# expands them at request time, so the package moves freely between drives.

cat > "$PKG/启动说明.txt" <<'EOF'
NonoClaw Windows 11 绿色版（免安装）
==================================

启动：解压整个目录后双击 NonoClaw.exe。可放在任意盘符/任意目录
（C:、D:、U盘均可）——配置内的路径是相对包自身的，移动后无需修改。

所有可写数据均在本目录：
  .nonoclaw\       当前配置、API Key、Skills、会话和记忆
  workspace\       默认项目工作目录
  .electron-data\  Electron 数据
目录不可写时应用会拒绝启动，不会回退写入用户目录。

内置 Node、Python（含 venv）、Rust 工具链（rustc/cargo/rustup）、Git、
rg、Poppler、MarkItDown、11 个当前 MCP，及 kiro-proxy。kiro-proxy 会随
NonoClaw 自动启动于 127.0.0.1:8000。
当前 settings.json（含 Key）和 Kiro 凭据已复制，无需重新输入。
需要联网或账号状态的 MCP 仍受网络、Token 有效期和 Windows 兼容性限制。

网络：所有本地服务仅监听 127.0.0.1 回环地址，不会触发 Windows 防火墙
管理员授权弹窗；外网访问仅由本机主动发起（HTTPS 出站）。

安全：本包包含私密 API Key 和 Kiro 凭据，请勿发送给其他人。
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
         "$RT/node-mcp/context7.js" "$RT/node-mcp/playwright.js" \
         "$RT/python-mcp/crawl4ai-proxy.py" "$RT/kiro-proxy/main.py" \
         "$RT/kiro-proxy/credentials.json" "$PKG/resources/app.asar" \
         "$PKG/templates/settings.json"; do
  [ -f "$f" ] || { echo "MISSING: $f"; exit 1; }
done
# numpy 2.x calls ucrtbase functions absent in Wine and breaks verification.
[ -d "$RT/python/Lib/site-packages/numpy" ] || { echo "MISSING: numpy"; exit 1; }
NP_VER=$(ls "$RT/python/Lib/site-packages/" | grep -oP '^numpy-\K[0-9.]+' | head -1)
case "$NP_VER" in 1.*) ;; *) echo "numpy version $NP_VER is not 1.x"; exit 1;; esac
# Ensure the packaged Electron startup code wires the backend to the SPA and
# enables the Windows software-rendering fallback (black-window regression).
rm -rf "$tmp/asar"
npx --yes @electron/asar extract "$PKG/resources/app.asar" "$tmp/asar"
grep -q 'NONOCLAW_DATA_DIR' "$tmp/asar/desktop/main.cjs" || { echo "packaged main missing static path"; exit 1; }
grep -q 'disableHardwareAcceleration' "$tmp/asar/desktop/main.cjs" || { echo "packaged main missing GPU fallback"; exit 1; }
# smoke: markitdown converts an HTML sample via wine (build-machine proof)
printf '<html><body><h1>pkgtest</h1></body></html>' > "$tmp/t.html"
timeout 180 wine "$PYEXE" -m markitdown "$(cygpath -w "$tmp/t.html" 2>/dev/null || echo "$tmp/t.html")" 2>/dev/null | grep -q "pkgtest" \
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
