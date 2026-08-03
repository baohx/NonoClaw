#!/usr/bin/env bash
# Setup script for the MarkItDown document converter integration.
# Creates a dedicated Python venv and installs the required packages.
# Run once:   bash scripts/setup_markitdown.sh
# Re-run to upgrade packages.

set -euo pipefail

VENV_DIR="${HOME}/.nonoclaw/venvs/markitdown"

echo "=== Setting up MarkItDown for NonoClaw ==="
echo "venv: ${VENV_DIR}"

# Find a suitable Python 3.10+ interpreter.
PYTHON=""
for candidate in python3.12 python3.11 python3.10 python3; do
    if command -v "${candidate}" &>/dev/null; then
        version=$("${candidate}" -c 'import sys; print(sys.version_info[:2])' 2>/dev/null || echo "")
        if [[ "${version}" == "(3, 1"* ]] || [[ "${version}" == "(3, 2"* ]]; then
            PYTHON="${candidate}"
            break
        fi
    fi
done

if [[ -z "${PYTHON}" ]]; then
    echo "ERROR: Python 3.10+ is required. Install it first:"
    echo "  sudo apt install python3.12 python3.12-venv"
    exit 1
fi

echo "Using Python: ${PYTHON} ($("${PYTHON}" --version))"

# Create venv if missing.
if [[ ! -d "${VENV_DIR}" ]]; then
    echo "Creating venv..."
    "${PYTHON}" -m venv "${VENV_DIR}"
fi

# Install/upgrade markitdown with document-format extras.
echo "Installing markitdown..."
"${VENV_DIR}/bin/pip" install --upgrade pip
"${VENV_DIR}/bin/pip" install 'markitdown[pdf,docx,pptx,xlsx]'

echo ""
echo "=== Done ==="
echo "MarkItDown installed at: ${VENV_DIR}/bin/markitdown"
"${VENV_DIR}/bin/markitdown" --version
echo ""
echo "NonoClaw will probe this path on next restart. No configuration needed."
echo "Set 'attachmentConverter: \"legacy\"' in settings.json to disable."
