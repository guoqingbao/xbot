---
name: skill-creator
description: Create or update xbot Agent Skills (SKILL.md format, metadata, layout, validation). Use when designing skills, authoring frontmatter, or packaging scripts, references, and assets.
metadata: {"xbot":{"emoji":"🛠️","triggers":["skill","skill creator","SKILL.md","frontmatter","metadata","packaging","agent skill"]}}
---

# Skill Creator (xbot)

This skill guides creation of **xbot** skills: modular packages that extend the agent with workflows, tool usage, and domain knowledge.

## About Skills

Skills are self-contained directories with a required `SKILL.md`. They act as onboarding for specific tasks—procedural knowledge the model should not have to rediscover each time.

### What Skills Provide

1. **Workflows** — Multi-step procedures for a domain
2. **Tool integrations** — CLIs, APIs, file formats
3. **Domain expertise** — Project or org-specific rules
4. **Bundled resources** — Optional `scripts/`, `references/`, `assets/`

### Core Principle: Concise Is Key

The context window is shared with the system prompt, history, other skills, and the user request. **Default assumption: the agent is already capable.** Only add what is non-obvious or procedural. Prefer short examples over long prose.

### Degrees of Freedom

- **High** — Multiple valid approaches; use natural-language instructions
- **Medium** — Preferred pattern with parameters; pseudocode or parameterized scripts
- **Low** — Fragile or sequence-critical work; small scripts with few knobs

---

## SKILL.md Format (YAML Frontmatter + Body)

Every `SKILL.md` **must** start with YAML between `---` delimiters, then Markdown body.

### Required keys

| Key | Meaning |
|-----|---------|
| `name` | Skill identifier. **Must match the parent directory name** (e.g. `my-skill` for `skills/my-skill/`). Lowercase letters, digits, hyphens only; ≤64 characters; no leading/trailing/double hyphens. |
| `description` | **Primary triggering signal.** Short, concrete summary of what the skill does and *when* to use it. This text is what agents see in the skills summary before loading the body—put “when to use” phrasing here, not only in the body. |

### Optional keys

| Key | Meaning |
|-----|---------|
| `metadata` | JSON object (often a single line). See **xbot metadata** below. |
| `always` | Boolean. If `true`, skill is treated as always-on (same effect can be set inside `metadata.xbot`). |
| `allowed-tools` | Comma-separated tool names; restricts which tools may be used with this skill when the runtime enforces it. |
| `homepage` | URL for humans or docs (optional). |

Example minimal frontmatter:

```yaml
---
name: pdf-tools
description: Extract and redact text from PDFs. Use when the user works with .pdf files, scanned documents, or asks for PDF text extraction or redaction.
metadata: {"xbot":{"emoji":"📄","triggers":["pdf","extract text","redact"],"requires":{"bins":["pdftotext"]}}}
---
```

---

## xbot `metadata` JSON

The `metadata` frontmatter value is JSON. xbot reads a **`xbot`** object first (fallbacks exist for other hosts; prefer `xbot` for this project).

```json
{
  "xbot": {
    "always": false,
    "triggers": ["keyword1", "keyword2"],
    "requires": {
      "bins": ["curl", "gh"],
      "env": ["GITHUB_TOKEN"],
      "os": ["darwin", "linux"]
    },
    "emoji": "📎"
  }
}
```

### Fields

- **`always`** (boolean) — When true, the skill is included in the “always” set (e.g. workspace-wide guardrails). Can also be set with top-level YAML `always: true`.
- **`triggers`** (array of strings) — Used for **suggestion matching**: if the user prompt (lowercased) **contains** any trigger substring, the skill may be suggested. Short, distinctive phrases work better than generic words.
- **`requires`** (object):
  - **`bins`** — CLI names that must exist on `PATH` for the skill to be marked available.
  - **`env`** — Environment variable names that must be set.
  - **`os`** — Allowed OS names (e.g. `darwin`, `linux`); current OS must match one if the array is non-empty.
- **`emoji`** (string) — Optional; for display or quick scanning in UIs.

Nested values in `requires` are merged for availability checks as implemented by xbot.

---

## Triggering: Description First, Triggers Second

1. **`description`** is shown in the skills list for every skill. It is the **main** hook—state clearly what the skill does and **exact situations** (user phrasing) when it applies.
2. **`triggers`** add substring matches against the user message for ranking or suggestions; they do **not** replace a bad description.

**Anti-pattern:** Putting the only copy of “when to use” inside the Markdown body. The body loads **after** the agent already chose the skill; the description must stand alone for selection.

---

## Progressive Disclosure

xbot uses a layered model:

1. **Summary** — `name`, `description`, availability/requirements (lightweight).
2. **SKILL.md body** — Loaded when the skill is relevant; keep under a few thousand words; split if needed.
3. **`scripts/` / `references/` / `assets/`** — Loaded or executed on demand; keeps the main file small.

**Patterns**

- Keep **one workflow overview** in `SKILL.md`; move long API tables to `references/api.md` and link to it.
- **Scripts** can be run without reading every line into context.
- **References** are for material the agent reads when a sub-task needs it.
- **Assets** are files used in outputs (templates, images), not necessarily loaded as text.

Avoid duplicating the same facts in both `SKILL.md` and a reference file—link instead.

---

## Directory Layout

```
skill-name/
├── SKILL.md          # required
├── scripts/          # optional; helper scripts
├── references/       # optional; docs, specs, long examples
└── assets/           # optional; templates, images, binaries for outputs
```

Rules enforced by xbot validation:

- **Only** these subdirectories are allowed at the skill root; no other folders or stray files (except `SKILL.md`).
- **No symlinks** in the skill directory or under allowed subdirectories.
- **`name` in frontmatter must equal the directory name.**

---

## Naming Conventions

- Lowercase **letters, digits, hyphens** only.
- **≤ 64** characters.
- **Directory name = `name` field** = how the skill is loaded (e.g. `skills/github-cli/SKILL.md` → `name: github-cli`).
- Prefer short, verb-led or tool-qualified names (`gh-pr-review`, `csv-transform`).

---

## Validation Rules (Checklist)

Before treating a skill as complete, verify:

1. `SKILL.md` exists and is readable, not a symlink.
2. YAML frontmatter is delimited by `---` at the top of the file.
3. `name` is non-empty, valid characters, length ≤ 64, **matches folder name**.
4. `description` is non-empty.
5. No forbidden extra files at skill root; only `scripts/`, `references/`, `assets/` besides `SKILL.md`.
6. `metadata` JSON parses and `xbot` object shape is intentional.

xbot exposes `validate_skill(skill_dir)` in code for programmatic checks; fix any reported issues before shipping.

---

## Creation Workflow (Manual)

1. **Examples** — Collect 2–3 real user requests that should hit this skill.
2. **Resources** — Decide if you need `scripts/`, `references/`, or `assets/`.
3. **Create directory** — `skills/<skill-name>/`.
4. **Write `SKILL.md`** — Frontmatter first (name, description, metadata); then concise body; link out to references.
5. **Test** — Run suggested flows; tighten description and triggers from misses.
6. **Iterate** — Adjust triggers and body from real usage.

---

## What Not to Add

Do not add README-only clutter at the skill root (`README.md`, `CHANGELOG.md`, etc.) unless the product explicitly requires it—the skill should be agent-oriented, not a human-only doc dump.

---

## Bundled Resources (Summary)

| Directory | Purpose |
|-----------|---------|
| `scripts/` | Deterministic helpers (shell, Python, …) |
| `references/` | Long docs, schemas, policies, deep examples |
| `assets/` | Output templates, images, boilerplate not meant to be fully inlined in context |

---

## Related Patterns (from Experience)

- **Multi-domain skills** — `SKILL.md` + `references/sales.md`, `references/finance.md`; only load the relevant reference.
- **Large reference files** — Add a table of contents at the top if longer than ~100 lines.
- **One level of indirection** — Link references from `SKILL.md` directly; avoid deep chains.

This structure aligns with how xbot lists skills, matches triggers, and loads full instructions on demand.
