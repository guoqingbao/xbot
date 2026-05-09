---
name: cron
description: "Schedule recurring or one-off work with the built-in cron tool (reminders, agent tasks, one-time runs)."
metadata: {"xbot":{"description":"cron tool: add/list/remove; intervals, cron expressions, one-shot at; timezones.","emoji":"⏰","triggers":["cron","schedule","reminder","timer","periodic"]}}
---

# Cron tool

The **`cron`** tool schedules jobs stored under `.xbot/cron/` (for example `jobs.json`). Each firing runs an **agent turn** with your message in the same channel/chat context that was active when the job was created. Results are delivered to the user when the run completes successfully.

Actions: `add`, `list`, `remove`.

You cannot create new jobs from inside a job execution (nested scheduling is blocked).

## Modes (how to think about the message)

All jobs use the same mechanism; the difference is how you phrase **`message`** and which schedule you pick:

1. **Reminder** — Short, self-contained text the user should see again (status, nudge, “standup in 5 minutes”). The agent processes it like any other turn; keep it readable as a notification.
2. **Task** — Instructions that require work each time (check a feed, run a report, summarize metrics). Make the message self-contained so the scheduled run does not depend on missing chat context.
3. **One-time** — Use the **`at`** parameter once; the job is removed after it runs (or disabled per internal rules). For repeating work, use **`every_seconds`** or **`cron_expr`** instead.

## Parameters (add)

| Parameter | Purpose |
|-----------|---------|
| `action` | `"add"` |
| `message` | Required. Body of the scheduled turn (name is derived from a short prefix). |
| `every_seconds` | Fixed interval in seconds (minimum 1). |
| `cron_expr` | Standard five-field cron string (minute hour dom month dow). A six-field form with seconds is accepted; five-field expressions are normalized with a leading `0` for seconds. |
| `tz` | **Only with `cron_expr`.** IANA name, e.g. `America/Vancouver`. Without `tz`, local server time is used. |
| `at` | One-shot: ISO datetime. Accepted: RFC3339 (e.g. `2026-04-01T15:30:00-07:00`) or local `YYYY-MM-DDTHH:MM:SS`. |

Exactly one of `every_seconds`, `cron_expr`, or `at` must be supplied for `add`.

## Time expression examples

| Intent | Illustration |
|--------|----------------|
| Every 20 minutes | `every_seconds`: `1200` |
| Every hour | `every_seconds`: `3600` |
| Daily at 08:00 (server local) | `cron_expr`: `"0 8 * * *"` |
| Weekdays 17:00 | `cron_expr`: `"0 17 * * 1-5"` |
| 09:00 weekdays in a specific zone | `cron_expr`: `"0 9 * * 1-5"`, `tz`: `"America/Vancouver"` |
| Once at a specific instant | `at`: RFC3339 or local ISO string as above |

## List and remove

```text
cron(action="list")
cron(action="remove", job_id="<id>")
```

Use `list` to copy the short `id` for removal.

## Timezone support

- **`tz`** applies **only** to **`cron_expr`** schedules. Invalid or unknown zone names are rejected.
- **`at`** is parsed as an absolute instant (RFC3339 includes offset) or local naive datetime—know which form you are passing.
- Interval schedules (`every_seconds`) are wall-clock intervals from the previous scheduling logic, not “9am every day” unless you use `cron_expr` + `tz`.

## See also

- **`scheduled-ops`** — Operational guidance for unattended recurring work.
