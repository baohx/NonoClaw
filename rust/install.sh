#!/usr/bin/env bash
# NonoClaw 安装脚本 — 编译并部署到 ~/.local/bin
#
# 用法:
#   bash install.sh                默认: dev 模式 (fast, ~3-5 min)
#   bash install.sh --release      正式版 (full LTO, ~20 min+)
#   bash install.sh --mold         dev + 安装 mold 快速链接器 (~1-2 min)
#   bash install.sh --release --mold  正式版 + mold 链接器 (~8-10 min)
#   bash install.sh --no-electron   跳过 Electron 桌面壳打包 (仅 CLI+Web UI)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
RUST_DIR="$SCRIPT_DIR"
FRONTEND_DIR="$PROJECT_DIR/frontend"
BIN_DIR="${NONOCLAW_BIN_DIR:-$HOME/.local/bin}"
BIN_DST="$BIN_DIR/nonoclaw"
DATA_DIR="${NONOCLAW_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/nonoclaw}"
FRONTEND_DST="$DATA_DIR/frontend/dist"

# ── 参数解析 ─────────────────────────────────────────────────────────
BUILD_MODE="dev"        # dev | release
INSTALL_MOLD=false
BUILD_ELECTRON=true
BUILD_WIN=false

for arg in "$@"; do
    case "$arg" in
        --release)      BUILD_MODE="release" ;;
        --mold)         INSTALL_MOLD=true ;;
        --no-electron)  BUILD_ELECTRON=false ;;
        --win)          BUILD_WIN=true ;;
        -h|--help)
            cat <<'HELP'
用法:
  bash install.sh                    默认: dev 模式 (fast, ~3-5 min)
  bash install.sh --release          正式版 (full LTO, ~20 min+)
  bash install.sh --mold             dev + 安装 mold 链接器 (~1-2 min)
  bash install.sh --release --mold   正式版 + mold 链接器 (~8-10 min)
  bash install.sh --no-electron      跳过 Electron 桌面壳打包
  bash install.sh --win              追加 Windows 桌面安装包 (mingw 交叉编译 + NSIS)
                                    依赖: x86_64-w64-mingw32-gcc, wine, wine32
HELP
            exit 0
            ;;
        *)
            echo "未知参数: $arg (可用: --release --mold)" >&2
            exit 1
            ;;
    esac
done

# ── 确定 profile ─────────────────────────────────────────────────────
if [[ "$BUILD_MODE" == "release" ]]; then
    CARGO_PROFILE="--release"
    BIN_SRC="$RUST_DIR/target/release/nonoclaw"
    MODE_LABEL="正式版 (LTO+单文件优化)"
else
    CARGO_PROFILE="--profile release-fast"
    BIN_SRC="$RUST_DIR/target/release-fast/nonoclaw"
    MODE_LABEL="开发版 (无LTO, 快速编译)"
fi
BIN_TMP=""
FRONTEND_TMP=""

cleanup() {
  if [ -n "$BIN_TMP" ] && [ -e "$BIN_TMP" ]; then
    rm -f "$BIN_TMP"
  fi
  if [ -n "$FRONTEND_TMP" ] && [ -d "$FRONTEND_TMP" ]; then
    rm -rf "$FRONTEND_TMP"
  fi
}
trap cleanup EXIT

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "错误: 缺少依赖 '$1'。请先安装后重试。" >&2
    exit 1
  fi
}

for command_name in cargo node npm mktemp install; do
  require_command "$command_name"
done

# ── Mold 链接器 (可选但强烈推荐) ─────────────────────────────────────
setup_mold() {
    if command -v mold &>/dev/null; then
        echo "  [OK] mold $(mold --version 2>&1 | head -1) 已安装"
        return 0
    fi
    if $INSTALL_MOLD; then
        echo "  安装 mold 快速链接器 ..."
        if command -v curl &>/dev/null; then
            MOLD_VER="2.36.0"
            ARCH=$(uname -m)
            TAR="mold-${MOLD_VER}-${ARCH}-linux.tar.gz"
            URL="https://github.com/rui314/mold/releases/download/v${MOLD_VER}/${TAR}"
            echo "  从 GitHub 下载 mold v${MOLD_VER} ..."
            curl -fsSL --connect-timeout 10 --max-time 120 "$URL" -o "/tmp/${TAR}" || {
                echo "  ✗ 下载失败，跳过 mold" >&2
                return 1
            }
            tar xzf "/tmp/${TAR}" -C /tmp
            sudo cp "/tmp/mold-${MOLD_VER}-${ARCH}-linux/bin/mold" /usr/local/bin/ 2>/dev/null || {
                echo "  ✗ 写入 /usr/local/bin 失败，跳过 mold" >&2
                return 1
            }
            rm -rf "/tmp/${TAR}" "/tmp/mold-${MOLD_VER}-${ARCH}-linux"
            echo "  [OK] mold $(mold --version 2>&1 | head -1) 安装完成"
        else
            echo "  ✗ 需要 curl，跳过 mold" >&2
            return 1
        fi
    else
        echo "  (未检出 mold 链接器 — 安装后编译可再快 2-3 倍)"
        echo "  下次运行: bash install.sh --mold"
        return 1
    fi
    return 0
}

# 写入 .cargo/config.toml 让 Rust 使用 mold
configure_mold_for_cargo() {
    if ! command -v mold &>/dev/null; then
        return
    fi
    local cfg="$RUST_DIR/.cargo/config.toml"
    mkdir -p "$(dirname "$cfg")"
    cat > "$cfg" <<'CARGOEOF'
# mold 快速链接器 (由 install.sh --mold 自动配置)
[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "link-arg=-fuse-ld=mold"]
CARGOEOF
    echo "  [OK] .cargo/config.toml 已配置 mold"
}

if [ ! -f "$RUST_DIR/Cargo.lock" ]; then
  echo "错误: 未找到 $RUST_DIR/Cargo.lock，无法执行锁定构建。" >&2
  exit 1
fi
if [ ! -f "$FRONTEND_DIR/package.json" ]; then
  echo "错误: 未找到 $FRONTEND_DIR/package.json。" >&2
  exit 1
fi
if [ ! -f "$FRONTEND_DIR/package-lock.json" ]; then
  echo "错误: 未找到 $FRONTEND_DIR/package-lock.json，无法执行可复现的 npm ci。" >&2
  exit 1
fi

printf '%s\n' "=== NonoClaw 安装 / Install$([ "$BUILD_MODE" = dev ] && echo '  [开发版 — 快速编译]' || echo '  [正式版 — 全量优化]') ==="
printf '项目目录 / Project: %s\n' "$PROJECT_DIR"
printf '二进制 / Binary:    %s\n' "$BIN_DST"
printf '前端 / Frontend:     %s\n\n' "$FRONTEND_DST"

echo "[0/5] 检查构建加速工具 / Setup build tools"
setup_mold
configure_mold_for_cargo
echo ""

echo "[1/5] 安装前端依赖并构建 / Install frontend dependencies and build"
cd "$FRONTEND_DIR"
# electron postinstall (inside npm ci) downloads from GitHub, which times out
# from CN networks. Mirror must be exported BEFORE npm ci runs.
ELECTRON_MIRROR="${ELECTRON_MIRROR:-https://npmmirror.com/mirrors/electron/}"
export ELECTRON_MIRROR
npm ci
npm run build
if [ ! -f "$FRONTEND_DIR/dist/index.html" ]; then
  echo "错误: 前端构建未生成 $FRONTEND_DIR/dist/index.html。" >&2
  exit 1
fi

echo "[2/5] 构建 CLI / Build CLI (profile: $CARGO_PROFILE)"
cd "$RUST_DIR"
START=$(date +%s)
cargo build $CARGO_PROFILE --locked --package nonoclaw
END=$(date +%s)
ELAPSED=$((END - START))
echo "  编译耗时: ${ELAPSED}s ($((ELAPSED / 60))m$((ELAPSED % 60))s)"
if [ ! -x "$BIN_SRC" ]; then
  echo "错误: 未生成可执行文件 $BIN_SRC。" >&2
  exit 1
fi

echo "[3/5] 复制可执行文件 / Copy executable"
mkdir -p "$BIN_DIR"
# The temporary file is on the destination filesystem, so mv performs an
# atomic replacement rather than leaving a source-tree symlink behind.
BIN_TMP="$(mktemp "$BIN_DIR/.nonoclaw.tmp.XXXXXX")"
install -m 0755 "$BIN_SRC" "$BIN_TMP"
mv -f "$BIN_TMP" "$BIN_DST"
BIN_TMP=""
if [ ! -x "$BIN_DST" ] || [ -L "$BIN_DST" ]; then
  echo "错误: 安装后的二进制无效或仍为符号链接: $BIN_DST。" >&2
  exit 1
fi

echo "[4/5] 复制前端资源 / Copy frontend assets"
mkdir -p "$DATA_DIR/frontend"
FRONTEND_TMP="$(mktemp -d "$DATA_DIR/frontend/.dist.tmp.XXXXXX")"
cp -R "$FRONTEND_DIR/dist/." "$FRONTEND_TMP/"
rm -rf "$FRONTEND_DST"
mv "$FRONTEND_TMP" "$FRONTEND_DST"
FRONTEND_TMP=""
if [ ! -f "$FRONTEND_DST/index.html" ]; then
  echo "错误: 安装后的前端缺少 $FRONTEND_DST/index.html。" >&2
  exit 1
fi

if [ "$BUILD_ELECTRON" = true ]; then
    echo "[5/5] 打包 Electron 桌面壳 / Package Electron desktop shell"
    cd "$FRONTEND_DIR"

    # Stage the freshly built binary for electron-builder extraResources.
    # electron-builder.yml references desktop/bin; works for both profiles.
    mkdir -p "$FRONTEND_DIR/desktop/bin"
    rm -f "$FRONTEND_DIR/desktop/bin/nonoclaw" "$FRONTEND_DIR/desktop/bin/nonoclaw.exe"
    install -m 0755 "$BIN_DST" "$FRONTEND_DIR/desktop/bin/nonoclaw"

    # npm ci wipes node_modules; extract-zip can silently fail on some Node
    # versions (only locales/ extracted). Self-heal: retry install.js with the
    # mirror (exported above), else unzip from the electron cache manually.
    if [ ! -x "$FRONTEND_DIR/node_modules/electron/dist/electron" ]; then
        echo "  electron 运行时缺失，重新下载 / electron runtime missing, re-downloading..."
        (cd "$FRONTEND_DIR/node_modules/electron" 2>/dev/null \
            && ELECTRON_MIRROR="$ELECTRON_MIRROR" node install.js) || true
        if [ ! -x "$FRONTEND_DIR/node_modules/electron/dist/electron" ]; then
            echo "  install.js 失败，尝试从缓存手工解压 / manual cache fallback..."
            ELECTRON_ZIP=$(find "$HOME/.cache/electron" -name 'electron-v*-linux-x64.zip' 2>/dev/null | sort -r | head -1)
            if [ -n "$ELECTRON_ZIP" ]; then
                rm -rf "$FRONTEND_DIR/node_modules/electron/dist"
                mkdir -p "$FRONTEND_DIR/node_modules/electron/dist"
                unzip -q -o "$ELECTRON_ZIP" -d "$FRONTEND_DIR/node_modules/electron/dist/"
                echo "electron" > "$FRONTEND_DIR/node_modules/electron/path.txt"
            fi
        fi
    fi
    if [ ! -x "$FRONTEND_DIR/node_modules/electron/dist/electron" ]; then
        echo "警告: electron 运行时不可用，跳过桌面壳打包 (可用 --no-electron 消除本警告)" >&2
    else
        ELECTRON_OUT="$FRONTEND_DIR/desktop/release/linux-unpacked"
        # If electron-builder fails (network/mirror flakiness), a stale
        # linux-unpacked from a previous run must NOT be presented as fresh.
        if ! npx electron-builder --dir --config electron-builder.yml; then
            echo "错误: electron-builder 打包失败 — 拒绝使用可能过期的旧产物。" >&2
            echo "      桌面壳保持上一次的状态；修复网络/镜像后重跑本脚本。" >&2
            echo "      (ELECTRON_MIRROR=https://npmmirror.com/mirrors/electron/ 通常可解)" >&2
            exit 1
        fi
        if [ -x "$ELECTRON_OUT/nonoclaw-frontend" ] && [ -x "$ELECTRON_OUT/resources/nonoclaw/bin/nonoclaw" ]; then
            # Freshness guard: electron-builder output must carry the exact
            # binary we just staged. A silent mismatch (partial build, stale
            # unpacked dir) is the #1 way to run an old backend unknowingly.
            STAGED_HASH=$(md5sum "$FRONTEND_DIR/desktop/bin/nonoclaw" | cut -d' ' -f1)
            DEPLOYED_HASH=$(md5sum "$ELECTRON_OUT/resources/nonoclaw/bin/nonoclaw" | cut -d' ' -f1)
            if [ "$STAGED_HASH" != "$DEPLOYED_HASH" ]; then
                echo "⚠️  打包产物二进制与 staging 不一致 — 原地修正" >&2
                # mv (atomic inode swap) succeeds even while a desktop instance
                # keeps the old file mapped; plain cp would fail with ETXTBSY.
                install -m 0755 "$FRONTEND_DIR/desktop/bin/nonoclaw" \
                    "$ELECTRON_OUT/resources/nonoclaw/bin/nonoclaw.fresh"
                mv -f "$ELECTRON_OUT/resources/nonoclaw/bin/nonoclaw.fresh" \
                    "$ELECTRON_OUT/resources/nonoclaw/bin/nonoclaw"
            fi
            # Warn if a desktop instance is still running the OLD image —
            # the new binary only takes effect after restarting the app.
            if pgrep -f "$ELECTRON_OUT/nonoclaw-frontend" >/dev/null 2>&1 \
                || pgrep -f "nonoclaw-frontend" >/dev/null 2>&1; then
                echo "提示: 桌面版正在运行 — 新二进制将在下次启动桌面版时生效。"
            fi
            # SUID-sandbox wrapper: unpacked builds lack SUID-root chrome-sandbox
            # and Ubuntu 23.10+ AppArmor blocks userns, so the launcher passes
            # --no-sandbox when the helper is not properly configured.
            install -m 0755 "$FRONTEND_DIR/desktop/launcher.sh" "$ELECTRON_OUT/nonoclaw-desktop"
            # Convenience symlink (idempotent)
            mkdir -p "$HOME/.local/bin"
            ln -sfn "$ELECTRON_OUT/nonoclaw-desktop" "$HOME/.local/bin/nonoclaw-desktop"
            # Chromium sandbox needs SUID-root chrome-sandbox; electron-builder
            # rebuilds the dir each time so the bit resets. Try sudo (NOPASSWD)
            # silently, else print the copy-paste command for the user.
            SANDBOX_HELPER="$ELECTRON_OUT/chrome-sandbox"
            if [ -f "$SANDBOX_HELPER" ]; then
                HELPER_MODE=$(stat -c '%a' "$SANDBOX_HELPER")
                HELPER_UID=$(stat -c '%u' "$SANDBOX_HELPER")
                if [ "$HELPER_UID" != "0" ] || [ $((8#${HELPER_MODE} & 8#4000)) -eq 0 ]; then
                    if sudo -n true 2>/dev/null; then
                        sudo chown root "$SANDBOX_HELPER" && sudo chmod 4755 "$SANDBOX_HELPER"
                        echo "✓ Chromium 沙箱已启用 (SUID 已自动配置)"
                    else
                        echo "提示: Chromium 沙箱未启用 (需 root)。桌面壳会以 --no-sandbox 降级运行。"
                        echo "      如需启用，请执行:"
                        echo "        sudo chown root '$SANDBOX_HELPER' && sudo chmod 4755 '$SANDBOX_HELPER'"
                    fi
                fi
            fi
            printf '✓ Electron 桌面壳: %s
' "$ELECTRON_OUT"
        else
            echo "警告: Electron 打包产物不完整，请检查上方 electron-builder 输出。" >&2
        fi

        # ── Windows 交叉编译 (--win) ──────────────────────────────
        if [ "$BUILD_WIN" = true ]; then
            echo "── Windows 桌面安装包 / cross-compile Windows installer ──"
            if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
                echo "错误: 缺少 mingw-w64 (sudo apt install mingw-w64)" >&2
                exit 1
            fi
            # The script's cwd is $FRONTEND_DIR here; cargo needs the workspace root.
            (cd "$RUST_DIR" && cargo build --release -p nonoclaw --target x86_64-pc-windows-gnu)
            # Stage ONLY the .exe (electron-builder filter matches both names,
            # so remove the Linux binary first to keep the win package clean).
            rm -f "$FRONTEND_DIR/desktop/bin/nonoclaw"
            install -m 0755 "$RUST_DIR/target/x86_64-pc-windows-gnu/release/nonoclaw.exe" \
                "$FRONTEND_DIR/desktop/bin/nonoclaw.exe"
            # rcedit.exe runs under wine; needs a user-owned WINEPREFIX
            # (the default ~/.wine may be broken from prior partial init).
            export WINEPREFIX="${WINEPREFIX:-$HOME/.wine-nonoclaw}"
            npx electron-builder --win nsis --x64
            WIN_EXE=$(ls -t "$FRONTEND_DIR/desktop/release/"nonoclaw-desktop-*.exe 2>/dev/null | head -1)
            if [ -n "$WIN_EXE" ]; then
                echo "✓ Windows 安装包: $WIN_EXE"
            else
                echo "警告: Windows 打包失败 (wine/rcedit 常见原因，见上方日志)。" >&2
            fi
            # Restore the Linux binary for subsequent local runs.
            install -m 0755 "$BIN_DST" "$FRONTEND_DIR/desktop/bin/nonoclaw"
            rm -f "$FRONTEND_DIR/desktop/bin/nonoclaw.exe"
        fi
    fi
else
    echo "[5/5] 跳过 Electron (--no-electron)"
fi

if printf '%s' "$PATH" | tr ':' '\n' | grep -qxF "$BIN_DIR"; then
  printf '✓ %s 已在 PATH 中 / is already on PATH\n' "$BIN_DIR"
else
  printf '提示 / Note: add this directory to PATH:\n  export PATH="%s:$PATH"\n' "$BIN_DIR"
fi

printf '\n=== 验证 / Verify ===\n'
"$BIN_DST" --version
BIN_SIZE=$(ls -lh "$BIN_DST" | awk '{print $5}')
printf '二进制大小: %s\n' "$BIN_SIZE"
printf '\n安装完成 / Installed. Start the Web UI with:\n  %s --serve-http 127.0.0.1:8765\n' "$BIN_DST"
printf '前端目录 / Frontend directory: %s\n' "$FRONTEND_DST"
printf 'CLI 二进制指纹: %s\n' "$(md5sum "$BIN_DST" | cut -d' ' -f1 | head -c 10)"
if [ "$BUILD_ELECTRON" = true ] && [ -x "$FRONTEND_DIR/desktop/release/linux-unpacked/nonoclaw-desktop" ]; then
    DESK_BIN="$FRONTEND_DIR/desktop/release/linux-unpacked/resources/nonoclaw/bin/nonoclaw"
    printf '桌面壳 / Desktop shell:  nonoclaw-desktop (→ ~/.local/bin/nonoclaw-desktop)\n'
    DESK_HASH=$(md5sum "$DESK_BIN" | cut -d' ' -f1)
    CLI_HASH=$(md5sum "$BIN_DST" | cut -d' ' -f1)
    if [ "$DESK_HASH" = "$CLI_HASH" ]; then
        printf '桌面后端指纹: %s (= CLI ✓)\n' "${DESK_HASH:0:10}"
    else
        printf '桌面后端指纹: %s (⚠️ 与 CLI 不一致!)\n' "${DESK_HASH:0:10}"
    fi
fi
