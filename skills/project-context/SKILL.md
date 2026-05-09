---
name: project-context
description: "Workspace template for project-specific architecture, commands, and conventions."
metadata: {"xbot":{"triggers":["project","context","architecture","overview"]}}
---

# Project Context Template

Fill this skill with repository-specific details that should be easy for the agent to load.

## Suggested Content

- architecture overview
- key directories and ownership
- build/test/lint commands
- deployment or release flow
- integration caveats

## Guidance

- Prefer concise bullets over large prose blocks.
- Keep this file updated when the project structure changes.
- Mirror durable facts into `memory/MEMORY.md` when they should remain active all the time.
