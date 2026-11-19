---
name: agentium-inbox
description: "Ask the user to decide an implementation question (spec gap, design choice between alternatives) through the Task Overview with `agentium todo ask`, then end the turn; or, at the end of a turn, hand the user your requests and proposals with `agentium todo add` instead of listing them in chat, and close items the user sent you with `agentium todo done`. Use when TERM_PROGRAM=agentium and you need the user's decision before continuing, are about to list remaining items, things for the user to do, or work you propose next, or received a `[from: Task Overview]` To do message. Not for permission to run commands, and not for questions you can settle from the code, the spec, or past decisions."
---

# Task Overview questions

Run only when `TERM_PROGRAM` is `agentium`. Otherwise ask in the conversation as usual.

## Ask

```bash
agentium todo ask "Close the monthly aggregate at JST or UTC midnight?" \
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

Without `--assume`, end your turn immediately. Do not keep working on an assumed answer. The user answers from Task Overview and the answer arrives in this tab as a new message starting with `[from: Task Overview]`; continue from there.

When the task's Task Coordinator runs in limited auto mode, it may answer instead; that answer starts with `[from: Task Coordinator]`. Treat it as a teammate's answer rather than the user's decision: follow it, but if it conflicts with the code, the spec or what the user told you, stop and ask again with `todo ask`.

The `Answer:` line carries the chosen option label, or the user's own text when you gave no options. A following `Note:` line is a condition or correction the user attached to that option; it takes precedence over the option's description.

With `--assume`, continue on the recommended option and mention the assumption in your final report. A message arrives only if the user picks another option (revise the work built on the assumption) or attaches a note to the assumed one (apply it). No message means the assumption stands.

If the command fails (e.g. `caller is not running inside an Agentium terminal or ACP tab`), ask the same question in the conversation instead.

# Agentium To do

The task's To do list in Task Overview: things the user must do, and work an agent does once the user says Go. Items belong to the task, not to your tab; each has an id, zero or more assigned agents, and a state (open, assigned, requested, done). Use it at the end of a turn, when you would otherwise list remaining items in chat. You do not wait on these; that is the difference from `todo ask`, which you use when you cannot continue without the answer.

```bash
agentium todo add --user-task "Check the Failed lane on screen :: stop a Codex ACP tab with an error; the lane is red"
agentium todo add --proposal "Add GPUI tests :: rebinding after restart, record aging, badge colors"
agentium todo add --proposal "Rewrite the billing job :: not my area" --unassigned
agentium todo list
agentium todo done 12 --note "pushed as 3f2a1c"
```

- `--user-task`: something only the user can do (verify on screen, configure, link). `--proposal`: work an agent does if the user says Go. Each is `TITLE :: DETAIL`, one item per `add`.
- An item you `add` is assigned to you and already requested: you are expected to finish it and run `todo done <id>`. Add `--unassigned` when it is not yours to do (another repository, another agent's area, or a human decision), so the user can assign it from Task Overview or another agent can `todo take <id>`.
- Aim for at most 5 new items per turn; a task holds at most 30 open items. Leave out FYIs and known limitations; those go in your final report.
- After adding, write only the count in chat (e.g. "4 items added to Task Overview To do"), not the items.
- If `todo add` fails (e.g. `caller is not running inside an Agentium terminal or ACP tab`), list the actual user tasks and proposals in chat and say they were not saved to Task Overview. Do not turn them into `todo ask` questions.
- Include things the user said they will do later (e.g. "I'll check the CI failure later") as user tasks, so they are not lost.
- `agentium todo list` prints the task's open items with ids, states and assignees (`--all` includes done ones), and this arena's pending questions. Run it after a compaction, or when a message refers to an item id.
- The user sends Go from Task Overview in one message starting with `[from: Task Overview]` and `To do`, one block per item, separated by blank lines: a `Go: #ID TITLE` line, then indented `:: DETAIL` and `Note: CONDITION` lines when present. A note belongs only to the item it is under; it is the user's condition and overrides the detail. Do the work, then `agentium todo done <id> --note "..."` for each item; items stay listed until you close them. Skip and Done answers never reach you: the user closes those items directly.
- Only an assigned agent may `done` or `update` an item (`todo take <id>` assigns you first). Anyone may change assignees: `todo assign <id> --to self`, `--to "<tab title>" [--arena <path>]`, or `--unassign`. This is a guard against closing someone else's work by accident, not a permission boundary.
- The Task Coordinator (a summarizer session the user enables per task) runs in no arena. Its `todo add` always creates unassigned items, it cannot `take` or be assigned items, its `todo list` covers the whole task's items and the questions of all its arenas, and `--to "<tab title>"` without `--arena` looks for the tab among the task's open arenas. In limited auto mode it also answers this task's questions (`agentium hub answer`), proposes work and orders the task's items (`todo sort`); it never sends Go, which stays with the user. Agents cannot run `todo sort`; `todo list` prints open items in that order, and `off task` marks items the order set aside as not needed to finish the task.
- `todo set` and `todo clear` no longer exist; there is no per-tab list to replace.
