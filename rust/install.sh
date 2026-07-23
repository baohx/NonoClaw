#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
RUST_DIR="$SCRIPT_DIR"
FRONTEND_DIR="$PROJECT_DIR/frontend"
BIN_SRC="$RUST_DIR/target/release/nonoclaw"
BIN_DIR="${NONOCLAW_BIN_DIR:-$HOME/.local/bin}"
BIN_DST="$BIN_DIR/nonoclaw"
DATA_DIR="${NONOCLAW_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/nonoclaw}"
FRONTEND_DST="$DATA_DIR/frontend/dist"
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

printf '%s\n' "=== NonoClaw 安装 / Install ==="
printf '项目目录 / Project: %s\n' "$PROJECT_DIR"
printf '二进制 / Binary:    %s\n' "$BIN_DST"
printf '前端 / Frontend:     %s\n\n' "$FRONTEND_DST"

echo "[1/4] 安装前端依赖并构建 / Install frontend dependencies and build"
cd "$FRONTEND_DIR"
npm ci
npm run build
if [ ! -f "$FRONTEND_DIR/dist/index.html" ]; then
  echo "错误: 前端构建未生成 $FRONTEND_DIR/dist/index.html。" >&2
  exit 1
fi

echo "[2/4] 构建锁定的 release CLI / Build locked release CLI"
cd "$RUST_DIR"
cargo build --release --locked --package nonoclaw
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
printf '\n安装完成 / Installed. Start the Web UI with:\n  %s --serve-http 127.0.0.1:8765\n' "$BIN_DST"
printf '前端目录 / Frontend directory: %s\n' "$FRONTEND_DST"
