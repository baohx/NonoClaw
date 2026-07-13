---
name: keybindings-help
description: Customize keyboard shortcuts, rebind keys, add chord bindings, or modify ~/.nonoclaw/keybindings.json.
when_to_use: when user wants to customize keyboard shortcuts, rebind keys, or change key bindings
---

# Keybindings Help

Customize keyboard shortcuts and key bindings.

## Default keybindings
- `Enter` — submit message
- `Ctrl+C` / `Esc` — cancel/interrupt
- `Ctrl+D` — exit
- `Up/Down` — navigate history
- `Ctrl+L` — clear screen

## Customization
Edit `~/.nonoclaw/keybindings.json` to rebind keys.

## Process
1. Ask the user which key they want to rebind
2. Read current keybindings if they exist
3. Propose the change
4. Write to `~/.nonoclaw/keybindings.json`
