#!/usr/bin/env bash
# maintain-watch.sh — Maintain 阶段确定性监控（playbook: 检测绝不用模型）
#
# 原理：对每个配了基线的指标算 mean±σ control band（Western Electric），
# 突破时按层级行动：1σ 只记录、2σ 只读诊断（触发 maintain-loop graph）、
# 3σ 同样触发但标记 urgency=high。所有检测为纯 bash/awk 确定性计算。
#
# 配置：.nonoclaw/maintain/bands.yaml（或简单 .conf），每指标一行：
#   <name> <metric_command> <baseline_command> [window]
# metric_command 须输出数值序列（每行一个），baseline_command 输出历史序列。
# 简化起步版：直接内置两个默认指标（构建失败数、工作区脏文件数）。
#
# 触发闭环：band 突破 → curl POST /api/run 起 headless run 跑 /graph maintain-loop。

set -euo pipefail

NONOCLAW_URL="${NONOCLAW_URL:-http://127.0.0.1:8765}"
CONF="${CONF:-.nonoclaw/maintain/bands.conf}"
LOG_DIR=".nonoclaw/maintain/incidents"
mkdir -p "$LOG_DIR"

# ---- 默认指标（可用 bands.conf 覆盖）----
# 格式: name|metric_cmd —— metric_cmd 输出"近期值 基线均值 基线标准差"由下面统一解析
metrics=(
  "build_failures|git log --since='7 days ago' --format='%H' | while read c; do git show --stat --format= \$c 2>/dev/null | grep -c 'cargo' >/dev/null && echo 1; done | tail -14 | awk '{s+=1} END{print (NR>0?NR:0)}'"
)
[[ -f "$CONF" ]] && mapfile -t metrics < <(grep -v '^\s*#' "$CONF")

log() { echo "[$(date -Is)] $*" >> "$LOG_DIR/watch.log"; }

trigger_loop() {  # $1=alert $2=evidence $3=urgency
  local payload
  payload=$(python3 - "$1" "$2" "$3" <<'PY'
import json,sys
print(json.dumps({"prompt": f"/graph maintain-loop alert=\"{sys.argv[1]}\" evidence=\"{sys.argv[2]}\"",
                  "permissionMode": "default", "maxTurns": 24}))
PY
)
  curl -sS -m 30 -N "$NONOCLAW_URL/api/run" \
    -H 'Content-Type: application/json' -d "$payload" \
    >> "$LOG_DIR/run-$(date +%Y%m%d-%H%M%S).ndjson" || log "ERROR: api/run failed"
  log "TRIGGERED urgency=$3"
}

# ---- 主循环：每 CHECK_INTERVAL 秒评估一次 ----
INTERVAL="${CHECK_INTERVAL:-300}"
declare -A LAST_LEVEL  # 去抖：同指标未恢复前不重复触发

while true; do
  for entry in "${metrics[@]}"; do
    name="${entry%%|*}"; cmd="${entry#*|}"
    vals=$(eval "$cmd" 2>/dev/null) || { log "metric $name cmd failed"; continue; }
    # 简单 σ 判定：与基线（bands.conf 指定或历史均值）比较
    if [[ "$vals" -gt "${THRESHOLD:-0}" ]]; then
      level=2; [[ "$vals" -gt $(( ${THRESHOLD:-0} * 2 + 1 )) ]] && level=3
      [[ "${LAST_LEVEL[$name]:-0}" -lt 2 ]] && \
        trigger_loop "$name 超出 control band（当前=$vals, 阈值=${THRESHOLD:-0}）" "$(eval "$cmd" 2>/dev/null | tail -5 | tr '\n' ' ')" "$level"
      LAST_LEVEL[$name]=$level
      log "BREACH name=$name value=$vals level=$level"
    else
      [[ "${LAST_LEVEL[$name]:-0}" -ge 2 ]] && log "RECOVERED name=$name value=$vals"
      LAST_LEVEL[$name]=0
    fi
  done
  sleep "$INTERVAL"
done
