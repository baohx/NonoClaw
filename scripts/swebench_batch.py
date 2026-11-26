#!/usr/bin/env python3
"""
SWE-bench batch runner for NonoClaw — with isolated uv venvs per repo.

Each repo gets its own venv (correct Python version), deps installed once,
then each task runs: agent → patch → evaluate tests in the venv.

Usage:
  python3 swebench_batch.py <task_id> [task_id2 ...] [--model MODEL] [--timeout SECS]
"""
import json, subprocess, sys, os, time

CYAN='\033[0;36m'; GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'; NC='\033[0m'
def log(m, c=''): print(f"{c}{m}{NC}", flush=True)

# ── Config ──
MODEL = 'deepseek-v4-pro'
AGENT_TIMEOUT = 300
MAX_TURNS = 20

# Python version per repo (SWE-bench official defaults)
REPO_PYVER = {
    'pylint-dev/pylint': '3.9',
    'psf/requests': '3.9',
    'pallets/flask': '3.9',
    'pydata/xarray': '3.10',
    'mwaskom/seaborn': '3.9',
    'pytest-dev/pytest': '3.9',
    'sphinx-doc/sphinx': '3.9',
    'django/django': '3.10',
    'sympy/sympy': '3.9',
}

# Extra pip packages per repo (beyond editable install)
REPO_EXTRAS = {
    'pylint-dev/pylint': ['pytest<8', 'pytest-cov'],
    'psf/requests': ['pytest<8', 'pytest-httpbin'],
    'pallets/flask': ['pytest<8', 'werkzeug>=2.3,<3.0', 'itsdangerous>=2.1,<3.0'],
    'pydata/xarray': ['pytest<8'],
    'mwaskom/seaborn': ['pytest<8', 'statsmodels', 'matplotlib'],
    'pytest-dev/pytest': ['pytest<8'],
    'sphinx-doc/sphinx': ['pytest<8'],
    'django/django': ['pytest<8'],
    'sympy/sympy': ['pytest<8'],
}

# Parse args
TASK_IDS = []
i = 1
while i < len(sys.argv):
    a = sys.argv[i]
    if a == '--model' and i+1 < len(sys.argv):
        MODEL = sys.argv[i+1]; i += 2
    elif a == '--timeout' and i+1 < len(sys.argv):
        AGENT_TIMEOUT = int(sys.argv[i+1]); i += 2
    elif a == '--max-turns' and i+1 < len(sys.argv):
        MAX_TURNS = int(sys.argv[i+1]); i += 2
    elif a.startswith('--'):
        i += 1
    else:
        TASK_IDS.append(a); i += 1

if not TASK_IDS:
    print("Usage: swebench_batch.py <task_id>... [--model M] [--timeout S]")
    sys.exit(1)

WORK_ROOT = '/tmp/swebench-work'
TASK_ROOT = '/tmp/swebench-tasks'

def sh(cmd, cwd=None, timeout=300):
    try:
        return subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return None

# venv cache: repo -> (venv_path, python_path)
venv_cache = {}

def get_venv(repo, work_dir, task_id):
    """Create/reuse a uv venv per task (isolated deps per base commit)."""
    pyver = REPO_PYVER.get(repo, '3.9')
    # Per-task venv: different base commits need different dependency sets
    venv_dir = f"{WORK_ROOT}/venvs/{task_id}"
    py = f"{venv_dir}/bin/python"

    if not os.path.exists(py):
        log(f"  Creating venv (Python {pyver}) for {task_id}...", YELLOW)
        r = sh(['uv', 'venv', '--python', pyver, venv_dir], timeout=120)
        if not r:
            log(f"  ⚠️ venv creation failed", RED)
            return None
        # bootstrap pip
        sh([py, '-m', 'ensurepip'], timeout=60)
        # install the repo editable + extras
        install_cmd = [py, '-m', 'pip', 'install', '--quiet', '-e', '.']
        for extra in REPO_EXTRAS.get(repo, []):
            install_cmd.append(extra)
        r = sh(install_cmd, cwd=work_dir, timeout=600)
        if r and r.returncode != 0:
            log(f"  ⚠️ dep install had issues (continuing anyway)", YELLOW)

    return (venv_dir, py)

results = []

for task_id in TASK_IDS:
    task_file = f"{TASK_ROOT}/{task_id}.json"
    if not os.path.exists(task_file):
        log(f"⚠️ Missing {task_file}", RED); continue
    task = json.load(open(task_file))
    repo = task['repo']; base_commit = task['base_commit']
    ftp = task['FAIL_TO_PASS']; ptp = task['PASS_TO_PASS'][:8]
    test_patch = task['test_patch']; golden = task['patch']

    log(f"\n{'═'*72}", CYAN)
    log(f"  {task_id}  ({repo})  model={MODEL}", CYAN)
    log(f"{'═'*72}", CYAN)

    work_dir = f"{WORK_ROOT}/{task_id}"

    # ── Setup repo ──
    log(f"[1/4] Repo setup...", YELLOW)
    if not os.path.exists(work_dir):
        r = sh(['git', 'clone', '--quiet', f'https://github.com/{repo}.git', work_dir], timeout=300)
        if not r:
            log(f"  ✗ clone failed", RED); results.append({'task':task_id,'resolved':False,'error':'clone'}); continue
    else:
        sh(['git', 'fetch', '--all'], cwd=work_dir, timeout=300)
    sh(['git', 'checkout', '-f', base_commit], cwd=work_dir)
    sh(['git', 'clean', '-fd'], cwd=work_dir)
    sh(['git', 'reset', '--hard', base_commit], cwd=work_dir)

    # ── venv + deps ──
    vp = get_venv(repo, work_dir, task_id)
    if not vp:
        results.append({'task':task_id,'resolved':False,'error':'venv'}); continue
    venv_dir, py = vp

    # ── baseline check ──
    log(f"[2/4] Baseline...", YELLOW)
    # Write test patch OUTSIDE work_dir so git clean doesn't delete it
    tp_file = f"{WORK_ROOT}/_patches/{task_id}.patch"
    os.makedirs(f"{WORK_ROOT}/_patches", exist_ok=True)
    open(tp_file,'w').write(test_patch)
    r = sh(['git','apply',tp_file], cwd=work_dir)
    if r is None or r.returncode != 0:
        sh(['git','apply','--3way',tp_file], cwd=work_dir)

    baseline_fail = True
    if ftp:
        r = sh([py,'-m','pytest',ftp[0],'-x','--tb=line','-q','--no-header'], cwd=work_dir, timeout=90)
        if r and r.returncode == 0:
            baseline_fail = False
            log(f"  ⚠️ baseline test PASSED unexpectedly (env issue?)", YELLOW)

    # reset test patch
    sh(['git','checkout','-f','--','.'], cwd=work_dir)
    sh(['git','clean','-fd'], cwd=work_dir)

    # ── Run agent ──
    log(f"[3/4] Agent ({MODEL})...", YELLOW)
    prompt = f"""You are working on the {repo} repository (checked out in the current directory).

## Issue
{task['problem_statement']}

## Instructions
1. Read the relevant source files to understand the codebase
2. Identify the root cause of the issue
3. Implement a minimal fix in the source code (do NOT modify test files)
4. Keep the fix minimal and follow existing code conventions

Do NOT run tests yourself — just make the source fix and briefly summarize it."""

    cmd = ['nonoclaw','-p','--permission-mode','bypassPermissions',
           '--max-turns',str(MAX_TURNS),'--output-format','json',
           '--model',MODEL,prompt]
    t0 = time.time()
    try:
        subprocess.run(cmd, cwd=work_dir, capture_output=True, text=True, timeout=AGENT_TIMEOUT)
    except subprocess.TimeoutExpired:
        pass
    at = time.time() - t0

    diff = sh(['git','diff'], cwd=work_dir)
    agent_patch = diff.stdout.strip() if diff else ''
    log(f"  agent {at:.0f}s, patch {len(agent_patch.splitlines())} lines", GREEN if agent_patch else RED)

    # ── Evaluate ──
    log(f"[4/4] Evaluate...", YELLOW)
    r = sh(['git','apply',tp_file], cwd=work_dir)
    if r is None or r.returncode != 0:
        sh(['git','apply','--3way',tp_file], cwd=work_dir)

    ftp_pass = ftp_total = len(ftp)
    for t in ftp:
        r = sh([py,'-m','pytest',t,'-x','--tb=line','-q','--no-header'], cwd=work_dir, timeout=90)
        if r is None or r.returncode != 0:
            ftp_pass -= 1
            log(f"    ❌ {t}", RED)
        else:
            log(f"    ✅ {t}", GREEN)

    ptp_pass = ptp_total = len(ptp)
    for t in ptp:
        r = sh([py,'-m','pytest',t,'-x','--tb=line','-q','--no-header'], cwd=work_dir, timeout=90)
        if r is None or r.returncode != 0:
            ptp_pass -= 1

    resolved = (ftp_pass == ftp_total) and (ptp_pass == ptp_total)
    result = {
        'task': task_id, 'repo': repo, 'resolved': bool(resolved),
        'agent_time_s': round(at,1),
        'agent_patch_lines': len(agent_patch.splitlines()) if agent_patch else 0,
        'golden_patch_lines': len(golden.splitlines()),
        'ftp_pass': f'{ftp_pass}/{ftp_total}', 'ptp_pass': f'{ptp_pass}/{ptp_total}',
        'baseline_fail': baseline_fail, 'pyver': REPO_PYVER.get(repo,'3.9'),
    }
    results.append(result)
    json.dump(result, open(f"{work_dir}/_result.json",'w'), indent=2)
    icon = '✅ RESOLVED' if resolved else '❌ UNRESOLVED'
    log(f"  → {icon} (ftp={ftp_pass}/{ftp_total}, ptp={ptp_pass}/{ptp_total}, {at:.0f}s)", GREEN if resolved else RED)

# ── Summary ──
log(f"\n{'═'*72}", CYAN)
log(f"  SUMMARY ({MODEL})", CYAN)
log(f"{'═'*72}", CYAN)
n = sum(1 for r in results if r['resolved'])
t = len(results)
if t:
    log(f"  Resolved: {n}/{t} ({n/t*100:.0f}%)", GREEN)
for r in results:
    icon = '✅' if r['resolved'] else '❌'
    err = f" [error={r.get('error')}]" if r.get('error') else ''
    log(f"  {icon} {r['task']:40s} ftp={r.get('ftp_pass','?'):6s} ptp={r.get('ptp_pass','?'):6s} {r.get('agent_time_s',0):5.0f}s py={r.get('pyver','?')}{err}")
json.dump(results, open(f"{WORK_ROOT}/_batch_results.json",'w'), indent=2)
log(f"\n  Saved to {WORK_ROOT}/_batch_results.json", CYAN)
