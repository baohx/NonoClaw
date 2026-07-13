---
name: simplify
description: Review changed code for reuse, simplification, efficiency, and altitude cleanups, then apply fixes.
when_to_use: after completing a feature, before merging, or when asked to "clean up" or "simplify" code
---

# Simplify

Review the changed code for reuse, simplification, efficiency, and altitude cleanups, then apply the fixes. Quality only — does not hunt for bugs; use `/code-review` for that.

## What to look for
1. **Dead code**: variables, functions, imports that are never used
2. **Duplication**: same logic in multiple places → extract helper
3. **Over-abstraction**: three similar lines don't need a strategy pattern
4. **Complexity**: deeply nested conditionals → early returns or guards
5. **Naming**: unclear variable/function names
6. **Unnecessary allocation**: clone() when a reference works, collect() then iterate

## Process
1. Get the diff: `git diff` or `git diff origin/main...HEAD`
2. Read each changed file
3. For each cleanup found: explain WHY it's better, then apply via Edit
4. Verify the build still passes

## What NOT to do
- Don't refactor unrelated code
- Don't introduce new abstractions "just in case"
- Don't change public APIs without reason
