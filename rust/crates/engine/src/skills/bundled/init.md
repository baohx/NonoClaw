---
name: init
description: Initialize a new CLAUDE.md file with codebase documentation for the current project.
when_to_use: when the user asks to "init", "set up CLAUDE.md", "create project docs", or on first use in a new project
---

# Init

Initialize a new CLAUDE.md file with codebase documentation.

## What CLAUDE.md contains
1. **Project overview**: what does this codebase do?
2. **Architecture**: key directories, modules, and their roles
3. **Build/test commands**: how to build, test, lint, run
4. **Code style**: naming conventions, patterns, idioms
5. **Dependencies**: key libraries and frameworks used

## Process
1. Explore the project structure (top-level files, key directories)
2. Read README, package.json / Cargo.toml, Makefile, etc.
3. Identify the build system and test framework
4. Read a few key source files to understand patterns
5. Write CLAUDE.md in the project root
6. Ask the user to review and adjust

## Guidelines
- Be concise — this is read by the model on every session
- Focus on what's non-obvious from reading the code
- Include exact build/test commands the model should run
- Update when the project structure changes
