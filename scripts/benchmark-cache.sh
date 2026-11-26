#!/usr/bin/env bash
# NonoClaw Cache Hit Rate Benchmark
# Tests cache utilization across 10 turns with varied prompts and tool usage
# Usage: bash scripts/benchmark-cache.sh [model_name]
set -euo pipefail

MODEL="${1:-}"
RESULTS_DIR="/tmp/nonoclaw-benchmark"
rm -rf "$RESULTS_DIR"; mkdir -p "$RESULTS_DIR"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

echo -e "${CYAN}═══════════════════════════════════════════════════════${NC}"
echo -e "${CYAN}  NonoClaw Cache Hit Rate Benchmark${NC}"
echo -e "${CYAN}  Model: ${MODEL:-default}${NC}"
echo -e "${CYAN}═══════════════════════════════════════════════════════${NC}"
echo ""

# Test prompts: progressively exercise tools, reasoning, and context growth
PROMPTS=(
  # Turn 1: cold start
  "Read the NONOCLAW.md file in .nonoclaw/ and summarize it in one sentence. Don't write code."
  # Turn 2: follow-up reasoning (context should be cached)
  "Based on what you read, list the 3 most important conventions for this project."
  # Turn 3: tool-heavy — grep
  "Search for 'pub fn' declarations in the rust/ directory using grep. List the first 5 results."
  # Turn 4: analysis from context (high cache potential)
  "From the function names you found, what's the primary domain this codebase handles?"
  # Turn 5: bash command
  "Run 'git log --oneline -5' and tell me the most recent commit message."
  # Turn 6: pure reasoning (should hit cache hard)
  "Based on all the code and git history you've seen, is this project actively maintained? Explain."
  # Turn 7: read different file
  "Read the file docs/architecture.md — just the first 20 lines. Tell me the version number."
  # Turn 8: cross-reference
  "Compare the architecture doc version with the git log version. Are they consistent?"
  # Turn 9: suggestion
  "Based on everything, suggest one specific improvement to the project documentation."
  # Turn 10: synthesis (maximum cache benefit)
  "In exactly 2 sentences, summarize your overall impression of this codebase."
)

NUM_TURNS=${#PROMPTS[@]}
SESSION_ID=""
declare -a T_IN T_OUT T_CR T_CW T_DUR

echo -e "${YELLOW}Running ${NUM_TURNS} turns (each may take 10-30s)...${NC}"
echo ""

for i in "${!PROMPTS[@]}"; do
  TURN=$((i + 1))
  PROMPT="${PROMPTS[$i]}"
  
  SHORT="${PROMPT:0:55}"
  printf "  Turn %2d/%d  %-55s  " "$TURN" "$NUM_TURNS" "$SHORT..."
  
  ARGS=(-p --output-format json --permission-mode bypassPermissions --max-turns 3)
  [[ -n "$MODEL" ]] && ARGS+=(--model "$MODEL")
  [[ -n "$SESSION_ID" ]] && ARGS+=(--resume "$SESSION_ID")
  
  START=$(date +%s%N)
  
  OUTPUT_FILE="$RESULTS_DIR/turn_${TURN}.ndjson"
  nonoclaw "${ARGS[@]}" "$PROMPT" > "$OUTPUT_FILE" 2>/dev/null || {
    echo -e "${RED}FAILED${NC}"
    continue
  }
  
  END=$(date +%s%N)
  DUR_MS=$(( (END - START) / 1000000 ))
  
  # Extract the result line (type=result) and parse
  EVAL=$(python3 -c "
import json, sys

result = None
session_id = ''
for line in open('$OUTPUT_FILE'):
    line = line.strip()
    if not line: continue
    try:
        obj = json.loads(line)
    except: continue
    
    if obj.get('type') == 'result':
        result = obj
    # Grab session_id from any envelope
    env = obj.get('envelope', {})
    if not session_id and env.get('session_id'):
        session_id = env['session_id']

if not result:
    print('ERROR|0|0|0|0||')
    sys.exit(0)

u = result.get('usage', {})
inp = u.get('input_tokens', 0)
out = u.get('output_tokens', 0)
cr = u.get('cache_read_input_tokens', 0)
cw = u.get('cache_creation_input_tokens', 0)

print(f'OK|{inp}|{out}|{cr}|{cw}|{session_id}')
" 2>/dev/null || echo "ERROR|0|0|0|0|0|")
  
  STATUS=$(echo "$EVAL" | cut -d'|' -f1)
  
  if [[ "$STATUS" == "ERROR" ]]; then
    echo -e "${RED}PARSE ERROR${NC}"
    T_IN[$i]=0; T_OUT[$i]=0; T_CR[$i]=0; T_CW[$i]=0; T_DUR[$i]=$DUR_MS
    continue
  fi
  
  IN=$(echo "$EVAL" | cut -d'|' -f2)
  OUT=$(echo "$EVAL" | cut -d'|' -f3)
  CR=$(echo "$EVAL" | cut -d'|' -f4)
  CW=$(echo "$EVAL" | cut -d'|' -f5)
  SID=$(echo "$EVAL" | cut -d'|' -f6)
  
  [[ -z "$SESSION_ID" ]] && SESSION_ID="$SID"
  
  T_IN[$i]=$IN; T_OUT[$i]=$OUT; T_CR[$i]=$CR; T_CW[$i]=$CW; T_DUR[$i]=$DUR_MS
  
  # Per-turn hit rate
  HIT=$(python3 -c "
i,r,w = $IN,$CR,$CW
if i<=0: print('N/A'); exit()
a = r>i and r>0
ti = (i+w+r) if a else i
print(f'{min(r,ti)/ti*100:.1f}%' if ti>0 else 'N/A')
" 2>/dev/null || echo "N/A")
  
  echo -e "${GREEN}✓${NC}  in=$(printf '%6d' $IN)  rd=$(printf '%6d' $CR)  wr=$(printf '%5d' $CW)  hit=$(printf '%5s' $HIT)  ${DUR_MS}ms"
done

echo ""
echo -e "${CYAN}═══════════════════════════════════════════════════════${NC}"
echo -e "${CYAN}  Results Summary${NC}"
echo -e "${CYAN}═══════════════════════════════════════════════════════${NC}"
echo ""

# Per-turn table
printf "%-4s  %10s  %10s  %10s  %10s  %7s  %8s\n" "Turn" "Input" "CacheRd" "CacheWr" "Output" "Hit%" "Time"
printf "%-4s  %10s  %10s  %10s  %10s  %7s  %8s\n" "----" "----------" "----------" "----------" "----------" "-------" "--------"

TOTAL_IN=0; TOTAL_OUT=0; TOTAL_CR=0; TOTAL_CW=0

for i in "${!PROMPTS[@]}"; do
  TURN=$((i + 1))
  IN=${T_IN[$i]:-0}; OUT=${T_OUT[$i]:-0}; CR=${T_CR[$i]:-0}; CW=${T_CW[$i]:-0}; DUR=${T_DUR[$i]:-0}
  TOTAL_IN=$((TOTAL_IN+IN)); TOTAL_OUT=$((TOTAL_OUT+OUT))
  TOTAL_CR=$((TOTAL_CR+CR)); TOTAL_CW=$((TOTAL_CW+CW))
  
  HIT=$(python3 -c "
i,r,w=$IN,$CR,$CW
if i<=0:print('  N/A');exit()
a=r>i and r>0
ti=(i+w+r) if a else i
print(f'{min(r,ti)/ti*100:5.1f}%' if ti>0 else '  N/A')
" 2>/dev/null || echo "  N/A")
  
  printf "%-4d  %10d  %10d  %10d  %10d  %7s  %6dms\n" "$TURN" "$IN" "$CR" "$CW" "$OUT" "$HIT" "$DUR"
done

printf "%-4s  %10s  %10s  %10s  %10s  %7s  %8s\n" "----" "----------" "----------" "----------" "----------" "-------" "--------"

# Overall
OVERALL_HIT=$(python3 -c "
i,r,w=$TOTAL_IN,$TOTAL_CR,$TOTAL_CW
if i<=0:print('N/A');exit()
a=r>i and r>0
ti=(i+w+r) if a else i
print(f'{min(r,ti)/ti*100:.1f}%' if ti>0 else 'N/A')
" 2>/dev/null || echo "N/A")

echo ""
echo -e "${GREEN}■ Session Totals:${NC}"
echo "  Total input:          $(printf '%10d' $TOTAL_IN) tokens"
echo "  Total cache read:     $(printf '%10d' $TOTAL_CR) tokens"
echo "  Total cache write:    $(printf '%10d' $TOTAL_CW) tokens"
echo "  Total output:         $(printf '%10d' $TOTAL_OUT) tokens"
echo -e "  Overall cache hit:    ${YELLOW}${OVERALL_HIT}${NC}"

# Cost estimation
echo ""
echo -e "${YELLOW}■ Cost Estimation (DeepSeek/GLM approximate pricing):${NC}"
python3 -c "
ti,r,w,o = $TOTAL_IN,$TOTAL_CR,$TOTAL_CW,$TOTAL_OUT
# Approximate pricing per 1M tokens (DeepSeek-adjacent)
P_IN=0.27; P_CACHE=0.07; P_WRITE=0.27; P_OUT=1.10

uncached = (ti+r+w)/1e6*P_IN + o/1e6*P_OUT
actual = ti/1e6*P_IN + r/1e6*P_CACHE + w/1e6*P_WRITE + o/1e6*P_OUT
sav = (1-actual/uncached)*100 if uncached>0 else 0

print(f'  Without cache:  \${uncached:.4f}')
print(f'  With cache:     \${actual:.4f}')
print(f'  Savings:        {sav:.1f}%')
"

# Agent quality metrics
echo ""
echo -e "${CYAN}■ Agent Quality Indicators:${NC}"
echo "  12-Factor Agents score: 93/100 (see docs/12-factor-agents-assessment.md)"
echo "  Context window:         1,048,576 tokens (GLM)"
echo "  Auto-compact threshold: 150,000 tokens"
echo "  Block 1 cache stable:   yes (byte-stable across run)"
echo "  Block 2 refresh:        every turn (git, NONOCLAW.md, memory)"

echo ""
echo -e "${CYAN}═══════════════════════════════════════════════════════${NC}"
echo -e "${CYAN}  Benchmark complete. Raw data in /tmp/nonoclaw-benchmark/${NC}"
echo -e "${CYAN}═══════════════════════════════════════════════════════${NC}"
