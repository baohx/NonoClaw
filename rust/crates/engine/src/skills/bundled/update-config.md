---
name: update-config
description: Configure the Claude Code harness via settings.json. Handle hooks, permissions, env vars, and other settings.
when_to_use: when user asks to change settings, add permissions, configure hooks, or modify settings.json
---

# Update Config

Configure settings via settings.json. Handles hooks, permissions, environment variables, and other configuration.

## Settings file locations
1. Project: `<cwd>/.nonoclaw/settings.json`
2. User: `~/.nonoclaw/settings.json`
3. CLI flags (highest priority)

## Common operations
- **Add permission**: `permissions.allow = ["Bash(npm *)", "Bash(cargo *)"]`
- **Add hook**: `hooks.PreToolUse = [{"matcher": "Bash", "command": "my-hook.sh"}]`
- **Set env var**: `env = {"NODE_ENV": "development"}`
- **Change model**: `model = "claude-sonnet-4-5-20250929"`
- **Set thinking**: `thinking = true`

## Process
1. Read the relevant settings file
2. Understand what the user wants to change
3. Propose the exact JSON change
4. Confirm with the user
5. Apply the change via Edit
6. Changes take effect on next session start (or immediately for some settings)
