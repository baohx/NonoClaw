---
name: code-review
description: Review the current diff for correctness bugs and reuse/simplification/efficiency cleanups.
when_to_use: before merging, after completing a feature, or when asked to "review this code"
---

# Code Review

Review the current diff for correctness bugs and cleanups.

## Two dimensions
1. **Correctness bugs**: logic errors, edge cases, null/error handling gaps, race conditions
2. **Quality cleanups**: dead code, duplication, over-abstraction, naming, complexity

## Process
1. Get the diff: `git diff` or `git diff origin/main...HEAD`
2. Read each changed file to understand context
3. Report findings in two groups:
   - **Bugs** (must fix): describe the bug, why it matters, how to fix
   - **Cleanups** (should fix): what and why
4. For each finding, provide file path and line reference

## Priority
- High: crash bugs, data loss, security issues
- Medium: wrong behavior, missing error handling
- Low: style, naming, minor duplication

## Output format
```
### Bugs
- [file:line] Description of bug → Proposed fix

### Cleanups
- [file:line] Description → Proposed improvement
```
