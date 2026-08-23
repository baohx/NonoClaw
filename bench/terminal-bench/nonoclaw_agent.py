"""NonoClaw agent for Terminal-Bench.

Drives the `nonoclaw` CLI inside the benchmark tmux session (the terminal
lives inside the task's Docker container, started by Terminal-Bench).

Execution model
---------------
Terminal-Bench gives us a fresh container per task. We provision it with the
NonoClaw binary + a minimal settings file, then run non-interactively with an
explicit permission bypass:

    timeout 900 nonoclaw -p --settings /tmp/nc_settings.json \
        --permission-mode bypassPermissions '<instruction>' \
        2>&1 | tee /tmp/nonoclaw_tb_run.log ; echo __NC_DONE_<id>__

Why `tee` instead of plain redirection: nonoclaw's output (including the final
`[turns: ...]` summary) is mirrored to the pane, so the poll loop can detect
completion from `capture_pane` text alone — no exec_run needed for detection.

Why `timeout` + the summary kill fallback: after finishing the task nonoclaw
may not exit inside tmux (shutdown hang, see
memory/facts/headless-tmux-shutdown-hang.md). Once the summary shows up in the
pane (task is genuinely done) and a minimum delay has passed, we SIGTERM the
lingering process inside the container so the shell reaches `; echo marker`
and the harness moves on. The task's files are already written by then, so the
run result is unaffected.

Token accounting: nonoclaw prints `[turns: N, in: N, out: N, ...]` at the end;
we parse it from the (tee'd) log so Terminal-Bench can report cost.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import time
from pathlib import Path

from terminal_bench.agents.base_agent import BaseAgent, AgentResult
from terminal_bench.terminal.tmux_session import TmuxSession

HOST_SETTINGS = Path.home() / ".nonoclaw" / "settings.json"
DEFAULT_MODEL = "deepseek-v4-pro"
DEFAULT_BASE_URL = "https://api.deepseek.com/anthropic"
# Per-model Anthropic-compatible endpoints (baseUrl must match the model's
# provider; a DeepSeek endpoint silently hangs/never answers for glm-* models).
BASE_URLS = {
    "deepseek": "https://api.deepseek.com/anthropic",
    "glm": "https://open.bigmodel.cn/api/anthropic",
    "claude": "https://api.anthropic.com",
}
BILLING_PROVIDERS = {
    "deepseek": "deepseek",
    "glm": "glm-coding",
    "claude": "anthropic",
}


def _base_url_for(model: str) -> str:
    m = model.lower()
    for prefix, url in BASE_URLS.items():
        if m.startswith(prefix) or prefix in m:
            return url
    return DEFAULT_BASE_URL


def _billing_provider_for(model: str) -> str:
    m = model.lower()
    for prefix, p in BILLING_PROVIDERS.items():
        if m.startswith(prefix) or prefix in m:
            return p
    return "deepseek"
# Statically-linked (glibc) build that runs inside the Debian-12 task
# containers; the dynamic ~/.local/bin build fails there (GLIBC_2.39 missing).
STATIC_BINARY = Path(__file__).resolve().parent / "bin" / "nonoclaw"
# Minimum wall-clock time before the "summary seen -> kill" fallback fires.
# Gives a well-behaved nonoclaw enough time to finish on its own (easy tasks
# take ~20-40s); the fallback only exists for the tmux shutdown hang.
_MIN_KILL_DELAY = 60.0

# NonoClaw prints a run summary like:
#   [turns: 2, in: 130, out: 35, cache read: 15872, cache write: 0]
_SUMMARY_RE = re.compile(r"\[turns:\s*(\d+),\s*in:\s*([\d,]+),\s*out:\s*([\d,]+)")


class NonoclawAgent(BaseAgent):
    """Terminal-Bench agent backed by the nonoclaw CLI."""

    def __init__(
        self,
        model: str | None = None,
        max_wait_seconds: int = 900,
        nonoclaw_binary: str | None = None,
        api_key: str | None = None,
        **kwargs,
    ):
        super().__init__(**kwargs)
        self._model = model or DEFAULT_MODEL
        self._max_wait_seconds = max_wait_seconds
        self._binary = Path(
            nonoclaw_binary
            or (STATIC_BINARY if STATIC_BINARY.exists() else shutil.which("nonoclaw"))
            or "/usr/local/bin/nonoclaw"
        )
        self._api_key = api_key or _load_api_key(self._model)

    @staticmethod
    def name() -> str:
        return "nonoclaw"

    # -- provisioning -------------------------------------------------------

    def _provision(self, session: TmuxSession) -> None:
        """Copy the binary + settings into the task container (fresh per task)."""
        if not self._api_key:
            raise RuntimeError(
                "DEEPSEEK_API_KEY not found in ~/.nonoclaw/settings.json or env"
            )
        if not self._binary.exists():
            raise RuntimeError(f"nonoclaw binary not found: {self._binary}")

        session.copy_to_container(
            [self._binary],
            container_dir="/usr/local/bin",
            container_filename="nonoclaw",
        )
        settings = {
            "promptProfile": "full",
            "models": [
                {
                    "name": self._model,
                    "label": self._model,
                    "baseUrl": _base_url_for(self._model),
                    "apiKey": self._api_key,
                    "role": ["main", "compact"],
                    "contextWindow": 1048576,
                    "maxTokens": 8192,
                    "billingProvider": _billing_provider_for(self._model),
                }
            ],
        }
        # Unique host temp path: concurrent trials must not clobber each
        # other's settings file (observed race when 3+ trials run at once).
        tmp = Path(f"/tmp/nc_settings_{os.getpid()}_{id(self)}.json")
        tmp.write_text(json.dumps(settings, indent=2))
        session.copy_to_container(
            [tmp], container_dir="/tmp", container_filename="nc_settings.json"
        )
        tmp.unlink(missing_ok=True)
        # NB: send_keys only blocks (and auto-appends Enter + `tmux wait`) when
        # the last key is Enter; always pass an explicit trailing "Enter".
        session.send_keys(["chmod +x /usr/local/bin/nonoclaw", "Enter"], block=True)

    def _build_command(self, instruction: str) -> str:
        cmd = [
            "timeout",
            str(self._max_wait_seconds),
            "nonoclaw",
            "-p",
            "--settings",
            "/tmp/nc_settings.json",
            "--permission-mode",
            "bypassPermissions",
        ]
        cmd.append(_shquote(instruction))
        # tee mirrors output to the pane so the poll loop detects completion
        # from capture_pane text alone (no exec_run needed for detection).
        return " ".join(cmd) + " 2>&1 | tee /tmp/nonoclaw_tb_run.log"

    # -- task execution -----------------------------------------------------

    def perform_task(
        self,
        instruction: str,
        session: TmuxSession,
        logging_dir: Path | None = None,
    ) -> AgentResult:
        self._provision(session)

        marker = f"__NC_DONE_{int(time.time())}__"
        shell = f"{self._build_command(instruction)}; echo {marker}"
        session.send_keys([shell, "Enter"], block=False)

        marker_re = re.compile(rf"(?m)^\s*{re.escape(marker)}\s*$")
        killed = False
        sent_at = time.monotonic()
        deadline = sent_at + float(self._max_wait_seconds) + 60
        print(f"[nonoclaw-agent] polling for {marker}", flush=True)
        while time.monotonic() < deadline:
            try:
                pane_text = session.capture_pane(capture_entire=True)
            except Exception as e:
                print(f"[nonoclaw-agent] capture_pane err: {e!r}", flush=True)
                pane_text = ""
            if marker_re.search(pane_text):
                print("[nonoclaw-agent] marker found", flush=True)
                break
            # nonoclaw finished (summary visible in the pane via tee) but the
            # process is stuck in tmux shutdown -> get the shell to return.
            if (
                not killed
                and time.monotonic() - sent_at > _MIN_KILL_DELAY
                and _SUMMARY_RE.search(pane_text)
            ):
                print("[nonoclaw-agent] summary in pane, killing", flush=True)
                self._kill_in_container(session)
                killed = True
            time.sleep(3)
        print("[nonoclaw-agent] poll done", flush=True)

        result = AgentResult(total_input_tokens=0, total_output_tokens=0)
        try:
            r = session.container.exec_run(["cat", "/tmp/nonoclaw_tb_run.log"])
            result.total_input_tokens, result.total_output_tokens = _parse_tokens(
                r.output.decode(errors="replace")
            )
        except Exception:
            pass
        return result

    def _kill_in_container(self, session: TmuxSession) -> None:
        """SIGTERM a lingering nonoclaw inside the task container.

        nonoclaw finishes the task (summary written) but may not exit inside
        tmux. We SIGTERM the lingering `nonoclaw`/`timeout` via /proc — pkill/ps
        are not installed in the slim task images. The shell then continues to
        `; echo marker` and the harness moves on.
        """
        # exec_run splits a str cmd into argv (no shell), so wrap in /bin/sh -c.
        # `[n]onoclaw` is the classic trick: the grep process's own cmdline
        # contains "[n]onoclaw", which the regex `[n]onoclaw` does NOT match —
        # so grep never kills itself mid-loop.
        kill_cmd = (
            "for p in /proc/[0-9]*; do "
            '[ "${p##*/}" = "$$" ] && continue; '
            "grep -qa [n]onoclaw $p/cmdline 2>/dev/null && "
            "kill -TERM ${p##*/} 2>/dev/null; done; true"
        )
        try:
            session.container.exec_run(["/bin/sh", "-c", kill_cmd], user="root")
        except Exception as e:
            print(f"[nonoclaw-agent] kill err: {e!r}", flush=True)
            try:
                session.send_keys(["C-c"], block=False)
            except Exception as e2:
                print(f"[nonoclaw-agent] ctrl-c err: {e2!r}", flush=True)


# -- helpers ----------------------------------------------------------------

API_KEY_ENV_NAMES = {
    "deepseek": "DEEPSEEK_API_KEY",
    "glm": "GLM_API_KEY",
    "claude": "ANTHROPIC_API_KEY",
}


def _api_key_env_for(model: str) -> str:
    m = model.lower()
    for prefix, env_name in API_KEY_ENV_NAMES.items():
        if m.startswith(prefix) or prefix in m:
            return env_name
    return "DEEPSEEK_API_KEY"


def _load_api_key(model: str | None = None) -> str | None:
    env_name = _api_key_env_for(model or DEFAULT_MODEL)
    if env := os.environ.get(env_name):
        return env
    try:
        d = json.loads(HOST_SETTINGS.read_text())
        return d.get("env", {}).get(env_name)
    except Exception:
        return None


def _shquote(s: str) -> str:
    return "'" + s.replace("'", "'\\''") + "'"


def _parse_tokens(text: str) -> tuple[int, int]:
    """Return (input, output) tokens from the run summary line.

    Only the `[turns: N, in: X, out: Y, ...]` summary counts — plain `\bout:`
    would also match nonoclaw's per-turn "Max out: 8192" log lines.
    """
    m = _SUMMARY_RE.search(text)
    if not m:
        return 0, 0
    return int(m.group(2).replace(",", "")), int(m.group(3).replace(",", ""))
