---
name: verify
description: Verify that a code change works by running the app and observing behavior.
when_to_use: when asked to verify a PR, confirm a fix works, test a change manually, or validate local changes before pushing
---

# Verify

Verify that a code change actually does what it's supposed to by running the app and observing behavior.

## When to use
- User asks "does this work?" or "test this"
- After implementing a feature or fix
- Before pushing or creating a PR

## Process
1. Identify the change: what was modified and what behavior it affects
2. Determine how to verify: run the app, hit an endpoint, check output
3. Execute the verification: use Bash to build/run, check logs
4. Report: did it work? What did you observe? Any issues?

## Key rules
- Actually RUN the code — don't just read it and assume
- If tests exist, run them first
- If the app needs to be built, build it
- Report actual output, not expectations
