---
name: security-review
description: Complete a security review of the pending changes on the current branch.
when_to_use: before merging sensitive changes, when asked to "security review", or for auth/crypto/data-handling code
---

# Security Review

Complete a security review of pending changes.

## Focus areas
1. **Injection**: SQL, command, path traversal — is user input sanitized?
2. **Authentication**: is auth checked on every endpoint? Token handling safe?
3. **Authorization**: can users access resources they shouldn't?
4. **Data exposure**: secrets in logs? PII in error messages? Debug endpoints exposed?
5. **Cryptography**: weak algorithms? Hardcoded keys? Non-cryptographic random for tokens?
6. **Dependencies**: new dependencies with known CVEs?
7. **Input validation**: boundary checks, type confusion, deserialization attacks

## Process
1. Get the diff
2. For each changed file, check against the focus areas above
3. Flag anything suspicious — false positive is better than missed vuln
4. For each finding: describe the vulnerability, exploit scenario, and fix

## Severity levels
- **Critical**: remote code execution, auth bypass, data breach
- **High**: privilege escalation, injection, sensitive data leak
- **Medium**: information disclosure, missing security headers
- **Low**: best practice deviations, defense-in-depth gaps
