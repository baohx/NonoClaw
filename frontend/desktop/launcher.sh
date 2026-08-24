#!/usr/bin/env bash
# NonoClaw Desktop launcher wrapper.
#
# The unpacked linux build ships chrome-sandbox without SUID-root (needs a
# one-time `sudo chown root chrome-sandbox && sudo chmod 4755 chrome-sandbox`),
# and Ubuntu 23.10+ AppArmor also blocks unprivileged user namespaces. So
# without root we must pass --no-sandbox to Chromium, and that flag must be
# present before Electron initializes (cannot be injected from JS main).
#
# If the SUID helper has been properly configured, we run with the sandbox.
set -euo pipefail

APP_DIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"

if [[ "$(uname -s)" == "Linux" && -z "${ELECTRON_SANDBOX_KEEP:-}" ]]; then
    helper="$APP_DIR/chrome-sandbox"
    if [ -f "$helper" ]; then
        mode=$(stat -c '%a' "$helper")
        owner=$(stat -c '%u' "$helper")
        # SUID-root (mode 4755, uid 0) → keep sandbox; otherwise disable.
        if [ "$owner" != "0" ] || [ $((mode & 4000)) -eq 0 ]; then
            # --disable-gpu: without the SUID sandbox, GPU child processes
            # fail to launch (error 1002) and Chromium eventually aborts with
            # "GPU process isn't usable". Software compositing is fine for a
            # chat UI. Dropped automatically when the sandbox is enabled.
            exec "$APP_DIR/nonoclaw-frontend" --no-sandbox --disable-gpu "$@"
        fi
    else
        # helper missing → same AppArmor/userns problem; run unsandboxed
        exec "$APP_DIR/nonoclaw-frontend" --no-sandbox --disable-gpu "$@"
    fi
fi
exec "$APP_DIR/nonoclaw-frontend" "$@"
