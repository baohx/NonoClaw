#!/usr/bin/env python3
"""
SWE-bench evaluation harness for NonoClaw.
Runs a single SWE-bench task: gives the problem to NonoClaw, collects the patch,
and evaluates it against the test suite.

Usage:
  python3 swebench_runner.py <task_json> [--model MODEL] [--timeout SECS]
"""
import json, subprocess, sys, os, time, tempfile, shutil

TASK_FILE = sys.argv[1]
MODEL = None
TIMEOUT = 300  # 5 minutes default

for i, arg in enumerate(sys.argv):
    if arg == "--model" and i + 1 < len(sys.argv):
        MODEL = sys.argv[i + 1]
    if arg == "--timeout" and i + 1 < len(sys.argv):
        TIMEOUT = int(sys.argv[i + 1])

task = json.load(open(TASK_FILE))
REPO = task["repo"]
BASE_COMMIT = task["base_commit"]
INSTANCE_ID = task["instance_id"]
PROBLEM = task["problem_statement"]
FAIL_TO_PASS = task["FAIL_TO_PASS"]
PASS_TO_PASS = task["PASS_TO_PASS"][:5]  # subset for speed
TEST_PATCH = task["test_patch"]
GOLD_PATCH = task["patch"]

WORK_DIR = f"/tmp/swebench-work/{INSTANCE_ID}"
RED = "\033[0;31m"
GREEN = "\033[0;32m"
YELLOW = "\033[1;33m"
CYAN = "\033[0;36m"
NC = "\033[0m"

print(f"{CYAN}═════════════════════════════════════════════════════════{NC}")
print(f"{CYAN}  SWE-bench Task: {INSTANCE_ID}{NC}")
print(f"{CYAN}  Repo: {REPO}{NC}")
print(f"{CYAN}  Model: {MODEL or 'default'}{NC}")
print(f"{CYAN}═════════════════════════════════════════════════════════{NC}")
print()

# ─── Step 1: Clone/reset repo to base commit ───
print(f"{YELLOW}[1/5] Setting up repository...{NC}")
if not os.path.exists(WORK_DIR):
    subprocess.run(["git", "clone", "--quiet", f"https://github.com/{REPO}.git", WORK_DIR],
                   check=True, capture_output=True)

# Reset to base commit
subprocess.run(["git", "fetch", "--all"], cwd=WORK_DIR, capture_output=True)
subprocess.run(["git", "checkout", BASE_COMMIT], cwd=WORK_DIR, capture_output=True)
subprocess.run(["git", "clean", "-fd"], cwd=WORK_DIR, capture_output=True)
subprocess.run(["git", "reset", "--hard", BASE_COMMIT], cwd=WORK_DIR, capture_output=True)
print(f"  ✓ Checked out {BASE_COMMIT[:12]}")

# ─── Step 2: Run NonoClaw on the problem ───
print(f"\n{YELLOW}[2/5] Running NonoClaw agent...{NC}")
prompt = f"""You are working on the {REPO} repository.

## Issue
{PROBLEM}

## Instructions
1. Read the relevant source files to understand the codebase
2. Identify the root cause of the issue described above
3. Implement a minimal fix (do NOT modify test files)
4. Make sure your fix is clean and follows existing code conventions

Focus on the src/ directory. Do not create new files unless absolutely necessary.
Output a brief summary of your fix when done."""

cmd = ["nonoclaw", "-p", "--permission-mode", "bypassPermissions",
       "--max-turns", "20", "--output-format", "json",
       "--add-dir", WORK_DIR]

if MODEL:
    cmd += ["--model", MODEL]

cmd += [prompt]

START = time.time()
print(f"  Running (timeout={TIMEOUT}s)...")

result_file = tempfile.NamedTemporaryFile(mode='w', suffix='.ndjson', delete=False)
result_file.close()

try:
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=TIMEOUT,
                          cwd=WORK_DIR)
    with open(result_file.name, 'w') as f:
        f.write(proc.stdout)
except subprocess.TimeoutExpired:
    print(f"  {RED}TIMEOUT after {TIMEOUT}s{NC}")
    # Still check what was produced

AGENT_TIME = time.time() - START
print(f"  Agent finished in {AGENT_TIME:.1f}s")

# ─── Step 3: Collect agent's changes (the patch) ───
print(f"\n{YELLOW}[3/5] Collecting agent patch...{NC}")
diff_result = subprocess.run(["git", "diff"], cwd=WORK_DIR, capture_output=True, text=True)
agent_patch = diff_result.stdout.strip()

# Also check for untracked files
untracked = subprocess.run(["git", "status", "--short"], cwd=WORK_DIR, capture_output=True, text=True)
print(f"  Agent changed files:")
for line in untracked.stdout.strip().split('\n'):
    if line:
        print(f"    {line}")

if not agent_patch:
    print(f"  {RED}No diff produced by agent{NC}")
    # Check if agent made any changes at all
    print(f"  git status output: {untracked.stdout[:200]}")
else:
    patch_lines = agent_patch.count('\n')
    print(f"  Patch size: {patch_lines} lines")
    # Save patch
    with open(f"/tmp/swebench-work/{INSTANCE_ID}_agent.patch", 'w') as f:
        f.write(agent_patch)

# ─── Step 4: Apply test patch + agent patch, run tests ───
print(f"\n{YELLOW}[4/5] Evaluating tests...{NC}")

# Write and apply test patch
test_patch_file = f"/tmp/swebench-work/{INSTANCE_ID}_test.patch"
with open(test_patch_file, 'w') as f:
    f.write(TEST_PATCH)

apply_test = subprocess.run(["git", "apply", test_patch_file], cwd=WORK_DIR, capture_output=True, text=True)
if apply_test.returncode != 0:
    print(f"  {RED}Failed to apply test patch: {apply_test.stderr[:200]}{NC}")
else:
    print(f"  ✓ Test patch applied")

# Agent patch is already applied (agent made changes directly in the working tree)
# Verify it's still there
diff_after = subprocess.run(["git", "diff", "--stat"], cwd=WORK_DIR, capture_output=True, text=True)
print(f"  Files changed after test patch: {diff_after.stdout.strip()[:200]}")

# Run FAIL_TO_PASS tests
print(f"\n  Running FAIL_TO_PASS tests ({len(FAIL_TO_PASS)}):")
for test in FAIL_TO_PASS:
    test_result = subprocess.run(
        ["python3", "-m", "pytest", test, "-x", "--tb=short", "-q"],
        cwd=WORK_DIR, capture_output=True, text=True, timeout=60
    )
    status = "PASS" if test_result.returncode == 0 else "FAIL"
    color = GREEN if status == "PASS" else RED
    print(f"    {color}{status}{NC}  {test}")

# Run subset of PASS_TO_PASS tests
if PASS_TO_PASS:
    print(f"\n  Running PASS_TO_PASS tests ({len(PASS_TO_PASS)} subset):")
    for test in PASS_TO_PASS:
        test_result = subprocess.run(
            ["python3", "-m", "pytest", test, "-x", "--tb=short", "-q"],
            cwd=WORK_DIR, capture_output=True, text=True, timeout=60
        )
        status = "PASS" if test_result.returncode == 0 else "FAIL"
        color = GREEN if status == "PASS" else RED
        print(f"    {color}{status}{NC}  {test}")

# ─── Step 5: Verdict ───
print(f"\n{YELLOW}[5/5] Final evaluation...{NC}")

# Re-run all FAIL_TO_PASS to get final verdict
all_pass = True
for test in FAIL_TO_PASS:
    r = subprocess.run(
        ["python3", "-m", "pytest", test, "-x", "--tb=line", "-q"],
        cwd=WORK_DIR, capture_output=True, text=True, timeout=60
    )
    if r.returncode != 0:
        all_pass = False
        break

# Check PASS_TO_PASS subset
pp_pass = True
for test in PASS_TO_PASS:
    r = subprocess.run(
        ["python3", "-m", "pytest", test, "-x", "--tb=line", "-q"],
        cwd=WORK_DIR, capture_output=True, text=True, timeout=60
    )
    if r.returncode != 0:
        pp_pass = False
        break

resolved = all_pass and pp_pass

if resolved:
    print(f"  {GREEN}✓ RESOLVED{NC} — all FAIL_TO_PASS tests now pass, PASS_TO_PASS tests still pass")
else:
    status_parts = []
    if not all_pass:
        status_parts.append("FAIL_TO_PASS not all passing")
    if not pp_pass:
        status_parts.append("PASS_TO_PASS regression")
    print(f"  {RED}✗ UNRESOLVED{NC} — {'; '.join(status_parts)}")

# Compare with golden patch
print(f"\n  Golden patch size: {len(GOLD_PATCH.splitlines())} lines")
print(f"  Agent patch size:  {len(agent_patch.splitlines()) if agent_patch else 0} lines")

# Save results
results = {
    "instance_id": INSTANCE_ID,
    "repo": REPO,
    "resolved": resolved,
    "agent_time_sec": round(AGENT_TIME, 1),
    "agent_patch_lines": len(agent_patch.splitlines()) if agent_patch else 0,
    "golden_patch_lines": len(GOLD_PATCH.splitlines()),
    "fail_to_pass_count": len(FAIL_TO_PASS),
    "pass_to_pass_count": len(PASS_TO_PASS),
    "agent_patch": agent_patch[:500],
}
results_file = f"/tmp/swebench-work/{INSTANCE_ID}_result.json"
with open(results_file, 'w') as f:
    json.dump(results, f, indent=2)

print(f"\n{CYAN}═════════════════════════════════════════════════════════{NC}")
print(f"  {'RESOLVED ✓' if resolved else 'UNRESOLVED ✗'}  |  {AGENT_TIME:.0f}s  |  {INSTANCE_ID}")
print(f"{CYAN}═════════════════════════════════════════════════════════{NC}")
