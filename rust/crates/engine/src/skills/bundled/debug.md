---
name: debug
description: Enable debug logging for this session to help diagnose issues.
when_to_use: when the user reports unexpected behavior, errors, or asks "why did that happen?"
---

# Debug

Enable debug logging to help diagnose issues in the current session.

## Process
1. Enable verbose output: set `RUST_LOG=debug` environment variable or the equivalent for the project
2. Reproduce the issue
3. Examine the logs for clues
4. Identify the root cause
5. Propose a fix

## For NonoClaw itself
- Run with `--verbose` flag to see debug logs on stderr
- Set `RUST_LOG=debug` for full tracing output
- Check `~/.nonoclaw/` for session files and settings

## For other projects
- Add debug prints or logging at the relevant code points
- Run with appropriate debug flags
- Check log files, stderr, and stdout
