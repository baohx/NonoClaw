#!/usr/bin/env bash
# requirements-sync.sh — V1: PR 合并后自动回写需求说明书 + 刷新代码库结构图
#
# 用法:
#   scripts/requirements-sync.sh [--pr N] [--base HEAD~1] [--dry-run] [--graph-only]
#
# 做三件事:
#   1. code-review-graph 增量刷新 (B13b) — 本地执行, 不走 LLM
#   2. 生成需求书同步 prompt, 经 CLI one-shot headless run
#      (--tag requirements-sync 隔离: auto-resume 和 dream 扫描均跳过)
#   3. run 结束后校验 REQUIREMENTS.md 是否有 diff (exit 2 = 需求书没动)
#
# 为什么是 CLI one-shot 而不是 REST /api/run: REST 是 control endpoint,
# 恒定要求 token (无 loopback 豁免, token 随机且不落盘), 脚本无法安全取得凭证。
# CLI 一次性行程 = 同一引擎管线, 无凭证问题, 且 --tag 隔离与 dream 同源。
#
# 挂接 git: 仓库跟踪 .githooks/post-merge (本仓库已设 core.hooksPath=.githooks)。
#   新 clone 激活: git config core.hooksPath .githooks
#   或 CI merge job 里直接调一行本脚本
# 手动触发等价物: 对 agent 说「同步需求说明书」(devflow trigger #9)

set -euo pipefail

PR_NUMBER=""
BASE="HEAD~1"
DRY_RUN=0
GRAPH_ONLY=0
MODEL="${SYNC_MODEL:-}"
NONOCLAW_BIN="${NONOCLAW_BIN:-nonoclaw}"
PY="${CODE_REVIEW_GRAPH_PY:-python3.12}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --pr) PR_NUMBER="$2"; shift 2 ;;
    --base) BASE="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --graph-only) GRAPH_ONLY=1; shift ;;
    --model) MODEL="$2"; shift 2 ;;
    *) echo "unknown flag: $1" >&2; exit 1 ;;
  esac
done

cd "$(git rev-parse --show-toplevel)"
REPO_NAME=$(basename "$PWD")
GIT_SHA=$(git rev-parse --short HEAD)

# 需求书位置: docs/ 被 gitignore 时回退仓库根 (devflow R8 纪律)
REQ_FILE="docs/REQUIREMENTS.md"
[[ -f "$REQ_FILE" ]] || REQ_FILE="REQUIREMENTS.md"
HAS_REQ=0
[[ -f "$REQ_FILE" ]] && HAS_REQ=1

echo "[requirements-sync] repo=$REPO_NAME sha=$GIT_SHA pr=${PR_NUMBER:-n/a} req=$REQ_FILE(has=$HAS_REQ) base=$BASE"

# ── 1. 结构图增量刷新 (B13b) — 无论如何都做, 机械操作 ─────────────────────
echo "[requirements-sync] refreshing code-review-graph..."
if "$PY" -m code_review_graph update --base "$BASE" --brief 2>&1 | tail -3; then
  echo "[requirements-sync] graph refreshed"
else
  echo "[requirements-sync] WARN: graph refresh failed (non-fatal, continuing)" >&2
fi

[[ $GRAPH_ONLY -eq 1 ]] && { echo "[requirements-sync] --graph-only, exiting"; exit 0; }

if [[ $HAS_REQ -eq 0 ]]; then
  echo "[requirements-sync] no REQUIREMENTS.md found — skipping LLM sync (nothing to sync)" >&2
  exit 3
fi

# ── 2. CLI one-shot headless run 同步需求书 (B13a) ────────────────────────
# 权限设计: headless 下 accept-edits 只放行 Read/Write/Edit, Bash 会被
# headless_resolve 自动 Ask→deny。因此 diff 摘要由脚本自己生成注入 prompt,
# run 内零 Bash 依赖 — "禁止 push" 从机制上成立, 不靠 prompt 约束。
COMMITS=$(git log --format='- %s' "$BASE"..HEAD 2>/dev/null | head -20 || true)
NUMSTAT=$(git diff --numstat "$BASE"..HEAD 2>/dev/null | head -80 || true)

PROMPT="你刚合并了 ${PR_NUMBER:+PR #${PR_NUMBER} }($GIT_SHA, base=$BASE)。
合并的提交清单:
$COMMITS

变更文件 (numstat 格式: 增行\\t删行\\t路径):
$NUMSTAT

任务: 同步需求说明书 $REQ_FILE。先 Read $REQ_FILE, 再用 Edit 修改功能点 checklist。
规则(devflow R8):
- 根据上述 diff 信息判断: 本次合并完成/新增/删除了哪些功能点, 对应勾选 [x] / 添加 / 删除条目; 需要细节可 Read 变更文件
- 需求书只装 Why/Who/What; 任何实施细节(How)混入则移出, 不新增
- 不改 Why/Who 部分, 除非 diff 明确显示用户群或动机变化
- 不确定的功能点宁可不勾选, 留待人工确认
- 完成后输出: 修改的功能点清单 + 一行变更摘要
- 纪律: 同一文件只 Read 一次; 最多读 6 个文件后必须开始 Edit; 不允许重复读取已读过的文件
只能用 Read/Edit 修改 $REQ_FILE; 禁止修改其他文件、执行命令、push"

if [[ $DRY_RUN -eq 1 ]]; then
  echo "[requirements-sync] DRY-RUN prompt:"
  echo "$PROMPT"
  exit 0
fi

if ! command -v "$NONOCLAW_BIN" >/dev/null 2>&1; then
  echo "[requirements-sync] nonoclaw CLI not found (set NONOCLAW_BIN) — aborting" >&2
  exit 4
fi

REQ_BEFORE=$(git hash-object "$REQ_FILE")
MODEL_ARGS=()
[[ -n "$MODEL" ]] && MODEL_ARGS=(--model "$MODEL")
"$NONOCLAW_BIN" \
  --tag requirements-sync \
  --permission-mode accept-edits \
  --max-turns 12 \
  "${MODEL_ARGS[@]}" \
  "$PROMPT"

# ── 3. 校验需求书确实被更新 ─────────────────────────────────────────────
REQ_AFTER=$(git hash-object "$REQ_FILE")
if [[ "$REQ_BEFORE" == "$REQ_AFTER" ]]; then
  echo "[requirements-sync] WARNING: $REQ_FILE unchanged after run — sync may have failed to identify changes" >&2
  exit 2
fi
echo "[requirements-sync] OK: $REQ_FILE updated (verify with: git diff $REQ_FILE)"
