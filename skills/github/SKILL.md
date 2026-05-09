---
name: github
description: "End-to-end GitHub workflows with gh: PRs, issues, reviews, branches, and releases."
metadata: {"xbot":{"description":"Broader GitHub practice with gh: workflows, triage, review, branching, releases.","emoji":"🐙","triggers":["github","pull request","issue","release","code review","gh"],"requires":{"bins":["gh"]}}}
---

# GitHub

This skill complements **`github-cli`** (command cheat sheet) with **workflow** guidance. All examples assume the [GitHub CLI](https://cli.github.com/) (`gh`) is installed and authenticated (`gh auth login`).

Scope:

- **`github-cli`** — Quick reference for common `gh` commands.
- **This skill** — How to use GitHub effectively: PR lifecycle, issues, review etiquette, branching, and releases.

Always pass **`--repo owner/repo`** when not inside a clone of that repository, or `cd` to the repo first.

## Pull requests

**Open and iterate**

- Branch from the default branch (or the team’s agreed base) with a descriptive name: `feat/…`, `fix/…`, `chore/…`.
- Keep PRs review-sized; split unrelated changes.
- Use the PR description to state **intent**, **scope**, and **how to test**; link issues with `Fixes #123` or `Refs #123` when applicable.

**CI and checks**

```bash
gh pr checks <number> --repo owner/repo
gh run list --repo owner/repo --limit 10
gh run view <run-id> --repo owner/repo --log-failed
```

Treat failing checks as blocking unless the team explicitly merges with red CI.

**Review response**

- Push follow-up commits or comment with resolution; resolve review threads when addressed.
- For stacked work, prefer dependent PRs or a single PR with clear commits—match what the repo already does.

## Issues

**Triage**

- Labels, milestones, and assignees create shared queues; prefer one primary assignee per active item.
- Reproduce bugs with version, environment, and minimal steps before deep fixes.

**Structured queries**

```bash
gh issue list --repo owner/repo --label bug --state open
gh issue view 42 --repo owner/repo
gh api repos/owner/repo/issues/42 --jq '.title, .body'
```

Use **`--json`** and **`--jq`** for dashboards and bots.

## Code review patterns

- Start with **intent**: does the change match the problem statement?
- Check **correctness**, **tests**, **API compatibility**, and **security** (secrets, injection, permissions).
- Prefer actionable comments (“consider X because Y”); avoid nit-only rounds on style if linters enforce it.
- Approve when you would be comfortable shipping; request changes when you would not.

## Branch strategies

Match the repository:

- **Trunk-based / short-lived branches** — Small PRs, fast merge, feature flags if needed.
- **GitFlow-style** — Long-lived `develop` and release branches; only use if the repo already does.
- **Fork PRs** — Common for OSS; sync default branch before large rebases.

Protect the default branch with required reviews and CI as appropriate.

## Release management

- **Versioning** — Follow SemVer unless the project defines otherwise; tag releases consistently.
- **Changelog** — Aggregate user-facing changes; link PRs and issues.
- **Artifacts** — Attach binaries or notes via `gh release create`, CI upload, or project-specific scripts.

```bash
gh release list --repo owner/repo --limit 5
gh release view v1.2.3 --repo owner/repo
```

Use **`gh api`** when you need endpoints not wrapped by a subcommand.

## JSON and automation

Most commands support **`--json`**; combine with **`--jq`** for scripts and summaries:

```bash
gh pr list --repo owner/repo --state merged --json number,title,mergedAt --jq '.[] | "\(.number)\t\(.title)"'
```

## Install `gh` (if missing)

- macOS: `brew install gh`
- Debian/Ubuntu: see [official install docs](https://github.com/cli/cli#installation) for apt and other packages.
