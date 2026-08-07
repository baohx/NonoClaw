#!/usr/bin/env bash
# NonoClaw 安装脚本 — 编译并部署到 ~/.local/bin
#
# 用法:
#   bash install.sh                默认: dev 模式 (fast, ~3-5 min)
#   bash install.sh --release      正式版 (full LTO, ~20 min+)
#   bash install.sh --mold         dev + 安装 mold 快速链接器 (~1-2 min)
#   bash install.sh --release --mold  正式版 + mold 链接器 (~8-10 min)

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

for arg in "$@"; do
    case "$arg" in
        --release)  BUILD_MODE="release" ;;
        --mold)     INSTALL_MOLD=true ;;
        -h|--help)
            cat <<'HELP'
用法:
  bash install.sh                    默认: dev 模式 (fast, ~3-5 min)
  bash install.sh --release          正式版 (full LTO, ~20 min+)
  bash install.sh --mold             dev + 安装 mold 链接器 (~1-2 min)
  bash install.sh --release --mold   正式版 + mold 链接器 (~8-10 min)
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

echo "[0/4] 检查构建加速工具 / Setup build tools"
setup_mold
configure_mold_for_cargo
echo ""

echo "[1/4] 安装前端依赖并构建 / Install frontend dependencies and build"
cd "$FRONTEND_DIR"
npm ci
npm run build
if [ ! -f "$FRONTEND_DIR/dist/index.html" ]; then
  echo "错误: 前端构建未生成 $FRONTEND_DIR/dist/index.html。" >&2
  exit 1
fi

echo "[2/4] 构建 CLI / Build CLI (profile: $CARGO_PROFILE)"
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

echo "[3/4] 复制可执行文件 / Copy executable"
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

echo "[4/4] 复制前端资源 / Copy frontend assets"
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
