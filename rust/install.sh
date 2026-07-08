#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
RUST_DIR="$SCRIPT_DIR"
FRONTEND_DIR="$PROJECT_DIR/frontend"
BIN_SRC="$RUST_DIR/target/release/nonoclaw"
BIN_DIR="${NONOCLAW_BIN_DIR:-$HOME/.local/bin}"
BIN_DST="$BIN_DIR/nonoclaw"
# Fixed location for the frontend bundle so `nonoclaw --serve-http` works from
# any directory. Follows XDG: $XDG_DATA_HOME/nonoclaw (defaults to ~/.local/share).
DATA_DIR="${NONOCLAW_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/nonoclaw}"
FRONTEND_DST="$DATA_DIR/frontend"

echo "=== NonoClaw 安装 ==="
echo "项目目录:   $PROJECT_DIR"
echo "Rust 源码:  $RUST_DIR"
echo "前端源码:   $FRONTEND_DIR"
echo "二进制目标: $BIN_DST"
echo "前端目标:   $FRONTEND_DST"
echo ""

# 1. 构建前端 (Vite → dist/)
if [ -f "$FRONTEND_DIR/package.json" ]; then
  echo "[1/4] 构建前端 (npm install + build)..."
  cd "$FRONTEND_DIR"
  if [ ! -d node_modules ]; then
    npm install
  fi
  npm run build
  echo "   → $FRONTEND_DIR/dist"
  echo ""
else
  echo "[1/4] 未找到前端 package.json，跳过前端构建"
  echo ""
fi

# 2. 构建 release 二进制
echo "[2/4] 构建 release 二进制..."
cd "$RUST_DIR"
cargo build --release
echo "   → $BIN_SRC"
echo ""

# 3. 安装二进制到 PATH
echo "[3/4] 安装 nonoclaw → $BIN_DST"
mkdir -p "$BIN_DIR"
if [ -L "$BIN_DST" ] || [ -e "$BIN_DST" ]; then
  echo "   → 已存在，覆盖"
  rm -f "$BIN_DST"
fi
ln -s "$(realpath "$BIN_SRC")" "$BIN_DST"
# 跨文件系统时符号链接可能失效，可改用硬拷贝:
# cp "$BIN_SRC" "$BIN_DST"
echo ""

# 4. 安装前端 dist 到固定路径 (匹配 serve_http 的 frontend_dir 查找)
echo "[4/4] 安装前端 → $FRONTEND_DST/dist"
mkdir -p "$FRONTEND_DST"
rm -rf "$FRONTEND_DST/dist"
if [ -d "$FRONTEND_DIR/dist" ]; then
  cp -r "$FRONTEND_DIR/dist" "$FRONTEND_DST/dist"
  echo "   → $FRONTEND_DST/dist/index.html"
else
  echo "   ⚠ 未找到 frontend/dist，前端未安装（--serve-http 将仅提供 WebSocket）"
fi
echo ""

# 检查 PATH
if echo "$PATH" | tr ':' '\n' | grep -qxF "$BIN_DIR"; then
  echo "✓ $BIN_DIR 已在 PATH 中"
else
  echo "⚠ $BIN_DIR 不在 PATH 中，请将下面这行加入 ~/.bashrc (或 ~/.zshrc):"
  echo ""
  echo "    export PATH=\"\$HOME/.local/bin:\$PATH\""
  echo ""
fi

# 验证
echo "=== 验证 ==="
"$BIN_DST" --version
echo ""
echo "安装完成！在任意目录执行即可启动 Web UI："
echo ""
echo "    nonoclaw --serve-http 127.0.0.1:8765"
echo ""
echo "配置文件: ~/.nonoclaw/settings.json"
