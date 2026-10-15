---
name: agentium-inbox
description: "Ask the user to decide an implementation question (spec gap, design choice between alternatives) through the Agentium Inbox with `agentium inbox ask`, then end the turn; or, at the end of a turn, hand the user your requests and proposals with `agentium todo set` instead of listing them in chat. Use when TERM_PROGRAM=agentium and you need the user's decision before continuing, or are about to list remaining items, things for the user to do, or work you propose next. Not for permission to run commands, and not for questions you can settle from the code, the spec, or past decisions."
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

# Agentium To do

Use at the end of a turn, when you would otherwise list remaining items in chat: things the user must do, or work you propose and the user should pick. The agent does not wait on these; that is the difference from `inbox ask`, which you use when you cannot continue without the answer.

```bash
agentium todo set \
  --user-task "Check the Failed lane on screen :: stop a Codex ACP tab with an error; the lane is red" \
  --proposal "Add GPUI tests :: rebinding after restart, record aging, badge colors" \
  --proposal "Push and open a PR :: goes outside this machine"
```

- `--user-task`: something only the user can do (verify on screen, configure, link). `--proposal`: work you would do if the user says go. Each is `TITLE :: DETAIL`; titles must be unique.
- `set` replaces this tab's whole list. Pass every item that still applies, not only the new ones. If the list is not in your context (e.g. after a compaction), run `agentium todo list` first.
- Read the output: a `Dropped ... unanswered items` line means you left out items. If they still apply, run `todo set` again with them.
- Aim for at most 5 items (8 is the hard limit). Leave out FYIs and known limitations; those go in your final report.
- After a successful `set`, write only the count in chat (e.g. "4 items in the Inbox To do"), not the items.
- If `todo set` fails (e.g. `caller is not running inside an Agentium terminal or ACP tab`), list the actual user tasks and proposals in chat and say they were not saved to the Inbox. Do not turn them into `inbox ask` questions.
- Include things the user said they will do later (e.g. "I'll check the CI failure later") as user tasks, so they are not lost.
- The user answers from the Inbox in one message starting with `[from: Agentium Inbox]` and `To do`, one line per answered item: `Done: N TITLE` (the user did it), `Go: N TITLE` (start it now), `Skip: N TITLE` (drop it; do not propose it again). A `— Note:` suffix is the user's condition and overrides the item's detail. Numbers are the positions in your list at that time; match items by title. Answered items are already removed from your list, so do not repeat them in the next `todo set`; unanswered ones are still there.
- `agentium todo clear` empties the list when nothing is left. Other tabs' lists in the same arena are shown by `todo list` and are read-only.
