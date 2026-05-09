---
name: scheduled-ops
description: "Recurring automation, cron-backed tasks, and unattended long-running bot operations."
metadata: {"xbot":{"triggers":["schedule","cron","timer","periodic"]}}
---

# Scheduled Operations

Use this workflow for unattended recurring work.

## Guidance

- Prefer descriptive job names.
- Make the message payload self-contained so the task can run without conversational context.
- For recurring reports, state where the report should be written or delivered.
- Avoid scheduling jobs from inside another cron execution unless the workflow explicitly requires it.

## Good Recurring Tasks

- repository health checks
- daily research reports
- backlog triage summaries
- dependency update scans
- regular status notifications
