---
name: loop
description: Run a prompt or slash command on a recurring interval.
when_to_use: when user wants to set up a recurring task, poll for status, or run something repeatedly
argument-hint: "[interval] <prompt or /command>"
arguments:
  - interval
---

# Loop

Run a prompt or slash command on a recurring interval.

## Usage
- `/loop 5m /deploy` — run /deploy every 5 minutes
- `/loop 30s check the build status` — check build every 30 seconds
- `/loop` (no args) — defaults to 10 minute interval

## Supported intervals
- `Xs` — seconds (e.g. `30s`)
- `Xm` — minutes (e.g. `5m`)  
- `Xh` — hours (e.g. `1h`)

## Process
1. Parse interval from first argument (default: 10m)
2. The remaining text is the prompt/command to repeat
3. Execute the prompt/command
4. Wait for the specified interval
5. Repeat until user cancels

## Notes
- The loop runs in the current session
- User can press Esc or Ctrl+C to cancel
- Each iteration is a fresh turn
