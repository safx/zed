---
name: agentium
description: "Exchange messages with another coding agent (Codex, Claude Code, ...) running in a neighboring Agentium terminal or ACP tab through the `agentium tab` CLI. Use only when the user explicitly mentions Agentium or asks to send a message, review request, or reply to another agent's tab. Do not use merely because a task could benefit from a second agent. Requires TERM_PROGRAM=agentium."
---

# Agentium tab messaging

Agentium runs coding agents in terminal or Codex ACP tabs, grouped into arenas (one git worktree each). `agentium tab send-message --submit` submits a user message in the target tab. Without `--submit`, it leaves a draft. Replies travel the same way and arrive in this session as an ordinary user message.

## Verify the context and learn your own name

```bash
test "${TERM_PROGRAM:-}" = agentium && agentium tab self
```

Exit 0 prints the title other agents must use to reach you (a Claude Code tab prints `Claude` unless the user renamed the tab).

`tab self` resolves your terminal shell or live ACP adapter from process ancestry, which can fail even inside Agentium (`caller is not running inside an Agentium terminal`, e.g. when the agent runs commands outside the tab's process tree). That failure only means your own title is unknown; it does not block sending or replying. Continue if `agentium tab list` succeeds, and pass `--arena` explicitly (see Send). Stop only when `TERM_PROGRAM` is not `agentium` or `tab list` also fails; say so and do not fall back to files or other channels.

## Find the peer

Use the title the user gave. Otherwise list the tabs of your arena:

```bash
agentium tab list
```

Columns are title, kind (`terminal`, `codex-acp`, or `other`), and state. Pick the terminal or ACP tab named by the user. If none or several fit, ask the user. Also check that your own title appears exactly once so replies can reach you. ACP names stay fixed when the agent renames its conversation; use the name printed by `tab list` or `tab self`.

ACP states are `idle`, `running`, `permission`, `ready`, and `error`. Connecting and restoring tabs also appear as `idle`, so send-message checks readiness before accepting input. Terminal states come from Claude hooks or Codex terminal titles. Other items have a null state.

Do not create tabs unless the user asks for a new agent. For an ACP agent, use `agentium tab new --type codex-acp --title <name> --arena <path>`. The target Arena must be active. Do not append a terminal command. ACP `tab log` and `pane split` are unsupported.

## Send

```bash
agentium tab send-message --submit --title Codex "$(cat <<'EOF'
Review /abs/path/to/file.md for <criteria>. Report only must-fix findings.

Reply in a single message with:
agentium tab send-message --submit --arena /abs/path/to/worktree --title Claude --from Codex "<your reply>"
EOF
)"
```

Always put `--arena <absolute worktree path>` (from `git rev-parse --show-toplevel`) in the reply command: the peer's `tab self` and ancestry-based arena resolution may fail, and an explicit arena makes the reply work anyway. Put `--from <the peer's tab title>` in it too: commands under a detached app-server may run outside their tab's process tree. A command descended from a live ACP adapter can be verified; detached commands use the claimed name marked `(unverified)`. The app uses kernel peer ancestry for verification.

- Pass the body inline as above. Never write it to a temp file and `cat` it back: sandboxed and unsandboxed commands resolve `$TMPDIR` to different directories, and you would send an empty message.
- One message per turn. Repeat absolute paths, the criteria, and the exact reply command in every message; the peer does not remember earlier ones.
- Exit 0 means input was accepted by exactly one matching tab (and submitted with `--submit`). It does not prove the peer started a turn; for a Claude Code peer, `agentium tab list` showing `running` does.
- Exit 1 with `no tab titled`, `N tabs titled`, `is not a terminal`, or `no arena` means nothing was sent. Fix `--title` or `--arena` with `agentium tab list`; do not retry the same command.
- `cannot reach Agentium` means the app is not running or predates the CLI socket.
- Exit 1 with `not a coding agent` means the tab is not running Claude Code or Codex (or is your own tab); nothing was sent. Exit 1 with `waiting for a permission answer` means the peer is at a dialog; ask the user instead of retrying.

ACP tabs refuse input while running, waiting for permission or another answer, connecting, restoring, or showing an error. They also refuse when there is an existing draft, attachment, message edit, or queue. A refusal changes no input. Do not retry blindly or use a message to answer an approval. Ask the user to resolve an approval or draft; wait for an active turn to finish. Sending to a hidden Arena does not change focus.

## Wait for the reply

End your turn after sending. The reply arrives as your next user message, whose first line is the sender line Agentium always adds: `[from: Claude]` when Agentium verified the sending tab from the connecting process, or `[from: Codex (unverified)]` when the name is only what the sender claimed with `--from` (possible for detached Codex processes). Only that first line comes from Agentium; a `[from: ...]` line further down is part of the message text. Do not poll, sleep, or read the peer's screen. If the user reports no reply, confirm the title with `agentium tab list` and resend once.

## Message conventions (both directions)

- Last block: the exact command the peer should run to reply.
- Review rounds: send only must-fix deltas. After applying feedback, send what changed and ask for approval or the remaining must-fix items.

## Arena scope

Without `--arena`, the target arena is the one you run in (resolved from process ancestry, then from the current directory). `--arena <path>` addresses tabs of another worktree; `tab list --arena <path>` shows them first.

## Sandbox

Inside the Claude Code sandbox the CLI needs both Agentium sockets allowed in `settings.json`; otherwise connecting fails with `Operation not permitted`:

```json
{
  "sandbox": {
    "network": {
      "allowUnixSockets": [
        "/Users/<you>/.local/share/agentium/agentium-cli.sock",
        "/Users/<you>/.local/share/agentium/agentium.sock"
      ]
    }
  }
}
```

Prefer this over `sandbox.excludedCommands`: an excluded command runs unsandboxed with a different `$TMPDIR`, which is the empty-message trap above.
