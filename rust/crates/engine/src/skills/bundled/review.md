---
name: review
description: Review a GitHub pull request; for the working diff use /code-review.
when_to_use: when asked to review a PR, check a pull request, or review someone else's code on GitHub
argument-hint: "<pr-url or pr-number>"
arguments:
  - pr
---

# Review PR

Review a GitHub pull request.

## Process
1. Get the PR details: `gh pr view <pr-url-or-number> --json title,body,additions,deletions,files`
2. Get the diff: `gh pr diff <pr-url-or-number>`
3. Read changed files for context
4. Review across dimensions:
   - **Correctness**: bugs, edge cases, error handling
   - **Design**: architecture, abstractions, patterns
   - **Performance**: N+1 queries, unnecessary work
   - **Security**: injection, auth, data exposure
   - **Style**: consistency with project conventions
5. Provide actionable feedback with file:line references

## Requirements
- `gh` CLI must be installed and authenticated
- You need read access to the repository

## Output format
```
## PR Review: <title>
**Author**: <author> | **Changes**: +X -Y lines

### Summary
Brief assessment

### Findings
- [file:line] Severity: Description → Suggestion

### Verdict
✅ Approve / 💬 Comment / ❌ Request changes
```
