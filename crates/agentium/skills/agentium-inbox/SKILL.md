---
name: agentium-inbox
description: "Ask the user to decide an implementation question (spec gap, design choice between alternatives) through the Agentium Inbox with `agentium inbox ask`, then end the turn. Use when TERM_PROGRAM=agentium and you need the user's decision before continuing. Not for permission to run commands, and not for questions you can settle from the code, the spec, or past decisions."
---

# Agentium Inbox questions

Run only when `TERM_PROGRAM` is `agentium`. Otherwise ask in the conversation as usual.

## Ask

```bash
agentium inbox ask "Close the monthly aggregate at JST or UTC midnight?" \
  --context "BillingAggregator.monthly needs a month boundary; spec §4 does not define it. DailyUsageJob cuts at JST; ai_usage.created_at is stored in UTC." \
  --option "JST 0:00 :: Matches the daily job and invoices. Adds one UTC→JST conversion in the query." \
  --option "UTC 0:00 :: Simplest query. The first 9 hours of each month fall into the previous month for Japanese spaces." \
  --recommended 1
```

- The question is one sentence naming the decision.
- `--context` says why you stopped: what the code or spec leaves open, with file and symbol names.
- Each `--option` is `LABEL :: DESCRIPTION`. The description states the impact and trade-off. Give 2–4 options.
- `--recommended` is the 1-based option you would pick, if any.
- Add `--assume` (requires `--recommended`) only when the recommended option is cheap to undo: you keep working on it instead of stopping. Do not assume when the choice is destructive or hard to reverse (schema changes, deletions, public API).

## After asking

Without `--assume`, end your turn immediately. Do not keep working on an assumed answer. The user answers from the Inbox and the answer arrives in this tab as a new message starting with `[from: Agentium Inbox]`; continue from there.

The `Answer:` line carries the chosen option label, or the user's own text when you gave no options. A following `Note:` line is a condition or correction the user attached to that option; it takes precedence over the option's description.

With `--assume`, continue on the recommended option and mention the assumption in your final report. A message arrives only if the user picks another option (revise the work built on the assumption) or attaches a note to the assumed one (apply it). No message means the assumption stands.

If the command fails (e.g. `caller is not running inside an Agentium terminal or ACP tab`), ask the same question in the conversation instead.
