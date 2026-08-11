#!/usr/bin/env bash
# 打包 NonoClaw 便携版（Windows 11）
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REL="$ROOT/release"
PKG="$REL/NonoClawPortable"
OUT="$REL/nonoclaw-v0.17.0-windows-portable.zip"

cd "$ROOT"

echo "=== 1. 校验核心产物 ==="
test -f "$PKG/nonoclaw.exe" || { echo "缺少 nonoclaw.exe"; exit 1; }
test -f "$PKG/frontend/dist/index.html" || { echo "缺少 frontend/dist"; exit 1; }
test -f "$PKG/node/node.exe" || { echo "缺少 node"; exit 1; }
test -f "$PKG/python/python.exe" || { echo "缺少 python"; exit 1; }
test -f "$PKG/.nonoclaw/settings.json" || { echo "缺少 settings.json"; exit 1; }
test -f "$PKG/start-nonoclaw.bat" || { echo "缺少启动器"; exit 1; }
test -f "$PKG/setup-first-run.bat" || { echo "缺少 setup"; exit 1; }
test -f "$PKG/安装说明.txt" || { echo "缺少安装说明"; exit 1; }
echo "  校验通过"

echo "=== 2. 清理可能残留 ==="
rm -rf "$PKG/.nonoclaw/venvs/markitdown"   # venv 由 setup-first-run.bat 在目标机创建
rm -f "$OUT"

echo "=== 3. 打包 zip（排除缓存/无用文件）==="
cd "$REL"
zip -rq "$OUT" NonoClawPortable \
  -x "NonoClawPortable/node/*.md" \
  -x "NonoClawPortable/node/CHANGELOG.md" \
  -x "NonoClawPortable/node/LICENSE" \
  -x "NonoClawPortable/node/README.md" \
  -x "NonoClawPortable/python/python312.pdb" \
  -x "NonoClawPortable/python/python3.pdb" \
  -x "NonoClawPortable/.nonoclaw/mcp-proxies/node_modules/.cache/*" \
  -x "*/__pycache__/*" \
  -x "*.DS_Store"

echo "=== 4. 完成 ==="
ls -lh "$OUT"
unzip -l "$OUT" | tail -1
