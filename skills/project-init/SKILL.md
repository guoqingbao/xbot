---
name: project-init
description: "Analyze a project workspace and generate a comprehensive RBOT.md context file for all agent instances."
metadata: {"rbot":{"triggers":["init","initialize","project init","setup project"]}}
---

# Project Init

Generate or refresh the `RBOT.md` file at the workspace root. This file is the shared project-scope context that every agent instance loads automatically.

## When This Skill Is Used

This skill is invoked when the user sends `/init` or `init` (or the keyword `[init]`). The agent must analyze the full project and produce a concise, high-signal `RBOT.md`.

## Output Target

Write the result to `{workspace}/RBOT.md`. If the file already exists, overwrite it with a fresh analysis.

## RBOT.md Structure

The file MUST follow this structure and stay within **300 lines**:

```
# {Project Name}

> One-line project summary.

## Tech Stack
- Language, framework, key libraries with versions

## Project Structure
- Top-level directory map with purpose of each directory
- Key files and their roles

## Architecture
- High-level component diagram (text-based)
- Data flow / request lifecycle
- Module boundaries and dependencies

## Key Components
- Core modules and their responsibilities
- Important traits / interfaces / abstractions
- Configuration system

## Development
### Build
- Build commands and prerequisites
### Test
- How to run tests, test structure
### Run
- How to start the application (all modes)

## Workflow
- Development workflow (branch strategy if visible)
- CI/CD if configured
- Deployment notes if available

## Key Facts
- Non-obvious conventions, gotchas, or constraints
- Important environment variables
- External service dependencies
```

## Rules

1. **300-line hard limit** — prioritize signal density over completeness.
2. **Accuracy over speculation** — only include facts verifiable from the codebase. If uncertain, omit rather than guess.
3. **Concrete over abstract** — use actual file paths, actual command names, actual module names.
4. **Stable references** — prefer paths and identifiers that won't change between commits.
5. **No code blocks longer than 10 lines** — this is a map, not a mirror.
6. **Skip empty sections** — if a section has no verifiable content, omit it entirely.

## Analysis Procedure

1. Read the project root: `Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, or equivalent to identify the tech stack.
2. List the top-level directory structure.
3. Identify and read key entry points (`main`, `lib`, `index`, `app`).
4. Trace the primary architecture: modules, traits/interfaces, data flow.
5. Read build/test/run configuration files.
6. Scan for CI/CD config (`.github/workflows/`, `Makefile`, `Dockerfile`, etc.).
7. Check for existing documentation (`README.md`, `docs/`, `ARCHITECTURE.md`).
8. Synthesize findings into the RBOT.md structure above.
9. Count lines — if over 300, compress the least critical sections.
10. Write the final `RBOT.md` to the workspace root.
