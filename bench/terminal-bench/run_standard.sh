#!/usr/bin/env bash
# Run the official Terminal-Bench harness against the NonoClaw agent.
#
# Requires:
#   - `tb` CLI (uv tool install terminal-bench)
#   - Docker (Terminal-Bench runs each task in a container)
#   - The NonoClaw `nonoclaw` binary on PATH inside the container image
#     (see README.md for the container setup)
#
# Usage:
#   ./run_standard.sh [extra tb args...]
#
# Examples:
#   ./run_standard.sh -t hello-world -t git-commit             # specific tasks
#   ./run_standard.sh --n-tasks 20                             # first 20 tasks
#   ./run_standard.sh -d terminal-bench-core==0.1.1            # pin dataset version

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export PYTHONPATH="$HERE${PYTHONPATH:+:$PYTHONPATH}"

MODEL="${NONOCLAW_TB_MODEL:-deepseek-v4-pro}"
MAX_WAIT="${NONOCLAW_TB_MAX_WAIT:-900}"
# Concurrency: 2 is the stable sweet spot. At 3+ the model's per-task response
# quality degrades and tasks loop until agent_timeout; at 1 it's just slower.
CONCURRENCY="${NONOCLAW_TB_CONCURRENCY:-2}"
# Local dataset (deployed via scripts/fetch_dataset.sh) — avoids GitHub clone
# timeouts. Override with DATASET_PATH=/path/to/tasks.
DATASET_PATH="${NONOCLAW_TB_DATASET:-$HOME/.cache/terminal-bench/terminal-bench-core/0.1.1/tasks}"

echo "=== Terminal-Bench run with NonoClaw agent ==="
echo "  model:       $MODEL"
echo "  max_wait:    $MAX_WAIT s"
echo "  concurrency: $CONCURRENCY"
echo "  dataset:     $DATASET_PATH"
if [ ! -d "$DATASET_PATH" ]; then
    echo "ERROR: dataset dir not found: $DATASET_PATH" >&2
    echo "Run scripts/fetch_dataset.sh first, or set NONOCLAW_TB_DATASET." >&2
    exit 1
fi

exec tb runs create \
  --agent-import-path nonoclaw_agent:NonoclawAgent \
  --agent-kwarg "model=$MODEL" \
  --agent-kwarg "max_wait_seconds=$MAX_WAIT" \
  --dataset-path "$DATASET_PATH" \
  --n-concurrent "$CONCURRENCY" \
  "$@"
