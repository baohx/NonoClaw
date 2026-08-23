#!/usr/bin/env python3
"""Local smoke harness for the NonoClaw Terminal-Bench agent.

Runs a small set of Terminal-Bench-style tasks against the `nonoclaw` CLI in a
host tmux session. No Docker required — this validates the agent integration
(finish signalling, token parsing, task completion) before running the real
`tb` harness inside containers.

Same execution model as `nonoclaw_agent.py`: nonoclaw is wrapped in `timeout`
with its output redirected to a log file, and a completion marker is echoed
after it. This keeps the tmux pane clean and guarantees the harness never
stalls even if nonoclaw lingers after a successful run.

Usage:
    python3 run_local_smoke.py                       # run all tasks
    python3 run_local_smoke.py --task write_file --max-wait 300
"""

from __future__ import annotations

import argparse
import re
import subprocess
import time
from dataclasses import dataclass, field

# --- task set ----------------------------------------------------------------

TASKS: dict[str, dict] = {
    "hello_world": {
        "instruction": (
            "Create a file called hello_world.py in the current directory. "
            "When run with python3, it must print the exact text 'Hello, World!'."
        ),
        "check": "[[ -f hello_world.py ]] && python3 hello_world.py | grep -q 'Hello, World!'",
        "difficulty": "easy",
    },
    "write_file": {
        "instruction": (
            "Create a file named greeting.txt whose contents are exactly the "
            "single line: hello from nonoclaw"
        ),
        "check": "grep -qx 'hello from nonoclaw' greeting.txt",
        "difficulty": "easy",
    },
    "grep_count": {
        "instruction": (
            "Count how many lines in /etc/passwd end with '/bin/bash' and write "
            "the number into a file named bash_users.txt"
        ),
        "check": (
            "expected=$(grep -c '/bin/bash$' /etc/passwd); "
            "test \"$(cat bash_users.txt)\" = \"$expected\""
        ),
        "difficulty": "medium",
    },
}


@dataclass
class TaskResult:
    name: str
    success: bool
    turns: int = 0
    in_tokens: int = 0
    out_tokens: int = 0
    duration_s: float = 0.0
    error: str = ""
    tail: str = field(default="", repr=False)

    def summary(self) -> str:
        status = "✅ PASS" if self.success else "❌ FAIL"
        return (
            f"{status}  {self.name:<14} turns={self.turns:>3} "
            f"in={self.in_tokens:>6} out={self.out_tokens:>6} "
            f"{self.duration_s:>7.1f}s"
        )


_SESSION = "nonoclaw_smoke"
_RUN_LOG = "/tmp/nonoclaw_tb_run.log"
_SUMMARY_RE = re.compile(r"\[turns:\s*(\d+),\s*in:\s*([\d,]+),\s*out:\s*([\d,]+)")


def _tmux(*args: str, timeout: float = 30.0) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["tmux", *args], capture_output=True, text=True, timeout=timeout
    )


def _run_task(name: str, task: dict, max_wait: int) -> TaskResult:
    started = time.monotonic()
    marker = f"__NC_SMOKE_{name}_{int(time.time())}__"
    _tmux("new-session", "-d", "-s", _SESSION, "-x", "200", "-y", "50", "/bin/bash")
    time.sleep(2)  # let the shell come up before sending keys
    try:
        shell_cmd = (
            f"timeout {max_wait} nonoclaw -p --permission-mode bypassPermissions "
            + "'" + task["instruction"].replace("'", "'\\''") + "'"
            + f" > {_RUN_LOG} 2>&1"
            + f"; echo {marker}"
        )
        _tmux("send-keys", "-t", _SESSION, shell_cmd, "Enter")

        # poll the pane for the completion marker (must appear as its own line,
        # not merely inside the echoed command line). If the run summary shows
        # up in the log but the marker does not, nonoclaw is stuck in shutdown
        # (known tmux-hang) — kill it so the harness can move on.
        marker_re = re.compile(rf"(?m)^\s*{re.escape(marker)}\s*$")
        text = ""
        deadline = time.monotonic() + float(max_wait) + 120
        killed = False
        while time.monotonic() < deadline:
            pane = _tmux("capture-pane", "-t", _SESSION, "-p", "-S", "-", "-E", "-")
            text = pane.stdout
            if marker_re.search(text):
                break
            if not killed:
                try:
                    log = open(_RUN_LOG, encoding="utf-8", errors="replace").read()
                    if _SUMMARY_RE.search(log):
                        # task finished; nonoclaw may be lingering in shutdown
                        subprocess.run(
                            ["pkill", "-TERM", "-f", r"^nonoclaw (-p|--print) "],
                            capture_output=True,
                        )
                        killed = True
                except OSError:
                    pass
            time.sleep(3)
        if not marker_re.search(text):
            _tmux("send-keys", "-t", _SESSION, "C-c")
            return TaskResult(
                name=name, success=False, error="agent timed out",
                duration_s=time.monotonic() - started,
            )

        # run the task check inside the same shell (sent verbatim)
        check = task["check"]
        _tmux("send-keys", "-t", _SESSION, f"echo __CHECK__ && {check} && echo __PASS__ || echo __FAIL__", "Enter")
        time.sleep(2.5)

        pane = _tmux("capture-pane", "-t", _SESSION, "-p", "-S", "-", "-E", "-")
        text = pane.stdout
        success = "__PASS__" in text and "__FAIL__" not in text.split("__PASS__")[-1]

        # parse tokens from the run log
        turns = in_tokens = out_tokens = 0
        try:
            log = open(_RUN_LOG, encoding="utf-8", errors="replace").read()
            m = _SUMMARY_RE.search(log)
            if m:
                turns = int(m.group(1))
                in_tokens = int(m.group(2).replace(",", ""))
                out_tokens = int(m.group(3).replace(",", ""))
        except OSError:
            pass

        return TaskResult(
            name=name,
            success=success,
            turns=turns,
            in_tokens=in_tokens,
            out_tokens=out_tokens,
            duration_s=time.monotonic() - started,
            tail="\n".join(text.splitlines()[-20:]),
        )
    finally:
        _tmux("kill-session", "-t", _SESSION)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--task", default=None, help="run a single task by name")
    ap.add_argument("--max-wait", type=int, default=300)
    args = ap.parse_args()

    names = [args.task] if args.task else list(TASKS)
    results: list[TaskResult] = []
    for name in names:
        task = TASKS.get(name)
        if task is None:
            print(f"unknown task: {name}")
            continue
        try:
            res = _run_task(name, task, args.max_wait)
        except subprocess.TimeoutExpired:
            res = TaskResult(name=name, success=False, error="tmux timeout")
            _tmux("kill-session", "-t", _SESSION)
        results.append(res)
        print(res.summary(), flush=True)
        if not res.success and res.tail:
            print("  --- tail ---")
            print("  " + res.tail.replace("\n", "\n  ")[-800:])

    if len(results) > 1:
        passed = sum(1 for r in results if r.success)
        print(f"\n=== {passed}/{len(results)} tasks passed ===")


if __name__ == "__main__":
    main()
