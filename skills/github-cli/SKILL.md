---
name: github-cli
description: "Interact with GitHub and CI systems through the gh CLI."
metadata: {"xbot":{"triggers":["github","git","pr","issue","repo"],"requires":{"bins":["gh"]}}}
---

# GitHub CLI

Use the `gh` CLI for GitHub issues, pull requests, workflow runs, and CI logs.

## Typical Commands

Check PR checks:

```bash
gh pr checks 55 --repo owner/repo
```

List workflow runs:

```bash
gh run list --repo owner/repo --limit 10
```

Inspect failed workflow logs:

```bash
gh run view <run-id> --repo owner/repo --log-failed
```

## Guidance

- Prefer `--json` and `--jq` for structured output.
- Use repository-qualified commands when not already in the target repository.
- Summarize failures by root cause, not just by step name.
