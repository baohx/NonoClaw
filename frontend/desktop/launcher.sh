#!/usr/bin/env bash
# NonoClaw Desktop launcher wrapper.
#
# The unpacked Linux build may ship chrome-sandbox without SUID-root (fix with
# a one-time `sudo chown root:root chrome-sandbox && sudo chmod 4755 chrome-sandbox`).
# If the helper cannot be used, fail closed. An operator can explicitly opt in
# to an unsandboxed one-off launch with NONOCLAW_ALLOW_UNSANDBOXED=1; this is
# never selected silently.
set -euo pipefail

APP_DIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"

run_unsandboxed_or_fail() {
    if [[ "${NONOCLAW_ALLOW_UNSANDBOXED:-}" == "1" ]]; then
        echo "WARNING: starting NonoClaw Desktop without the Chromium sandbox by explicit request." >&2
        exec "$APP_DIR/nonoclaw-frontend" --no-sandbox --disable-gpu "$@"
    fi
    cat >&2 <<'EOF'
NonoClaw Desktop refused to start because the Chromium sandbox helper is not
SUID-root. Fix the packaged helper, then retry:

  sudo chown root:root chrome-sandbox
  sudo chmod 4755 chrome-sandbox

To accept the risk for this launch only, set NONOCLAW_ALLOW_UNSANDBOXED=1.
EOF
    exit 1
}

if [[ "$(uname -s)" == "Linux" && -z "${ELECTRON_SANDBOX_KEEP:-}" ]]; then
    helper="$APP_DIR/chrome-sandbox"
    if [ -f "$helper" ]; then
        mode=$(stat -c '%a' "$helper")
        owner=$(stat -c '%u' "$helper")
        # SUID-root (mode 4755, uid 0) keeps the sandbox; otherwise fail
        # closed unless the operator explicitly opts into the unsafe mode.
        if [[ "$owner" != "0" ]] || (( (8#$mode & 8#4000) == 0 )); then
            # --disable-gpu: without the SUID sandbox, GPU child processes
            # fail to launch (error 1002) and Chromium eventually aborts with
            # "GPU process isn't usable". Software compositing is fine for a
            # chat UI. Dropped automatically when the sandbox is enabled.
            run_unsandboxed_or_fail "$@"
        fi
    else
        # Missing helper follows the same fail-closed policy.
        run_unsandboxed_or_fail "$@"
    fi
fi
exec "$APP_DIR/nonoclaw-frontend" "$@"
