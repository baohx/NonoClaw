---
name: remember
description: Review auto-memory entries and propose upgrades to CLAUDE.md, CLAUDE.local.md, or shared memory.
when_to_use: after a session ends, or when the user says "remember this" or "save this for later"
---

# Remember

Review the session's auto-memory entries and propose upgrades.

## Memory hierarchy
1. **MEMORY.md** — auto-extracted facts, per-project `.nonoclaw/memory/MEMORY.md`
2. **CLAUDE.md** — project-level instructions, always loaded
3. **~/.nonoclaw/CLAUDE.md** — user-level instructions, always loaded

## Process
1. Read current MEMORY.md if it exists
2. Review the conversation for durable facts worth remembering:
   - User preferences (naming style, tool choices)
   - Project conventions discovered
   - Repeated corrections the user made
3. Propose new memory entries with frontmatter (name, description, type)
4. Ask the user to confirm before writing
5. Write to MEMORY.md using the Write tool

## Memory entry format
```markdown
---
name: short-kebab-case-slug
description: one-line summary
metadata:
  type: user | feedback | project | reference
---

The fact. **Why:** reason. **How to apply:** guidance.
```
