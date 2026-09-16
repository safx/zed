---
name: agentium
description: "Exchange messages with another coding agent (Codex, Claude Code, ...) running in a neighboring Agentium terminal tab through the `agentium tab` CLI. Use only when the user explicitly mentions Agentium or asks to send a message, review request, or reply to another agent's tab. Do not use merely because a task could benefit from a second agent. Requires TERM_PROGRAM=agentium."
---

# Agentium tab messaging

Agentium runs coding agents in terminal tabs, grouped into arenas (one git worktree each). `agentium tab send-message` pastes text into another tab and presses Enter, so the agent there receives it as a user message. Replies travel the same way and arrive in this session as an ordinary user message.

## Verify the context and learn your own name

```bash
test "${TERM_PROGRAM:-}" = agentium && agentium tab self
```

Exit 0 prints the title other agents must use to reach you (a Claude Code tab prints `Claude` unless the user renamed the tab). Any failure means you are not inside an Agentium terminal or Agentium is not running; say so and stop. Do not fall back to files or other channels.

## Find the peer

Use the title the user gave. Otherwise list the tabs of your arena:

```bash
agentium tab list
```

Columns are title, kind (`terminal` or `other`), and state. Pick the terminal tab named after the target agent, such as `Codex`. If none or several fit, ask the user. `state` (`permission`, `running`, `ready`, `idle`) comes from Claude Code hooks for Claude tabs and from the terminal title (spinner, "Action Required") for Codex tabs; for other tabs it is always `idle` and carries no information.

Do not create tabs (`agentium tab new --title Codex -- codex`) unless the user asks for a new agent.

## Send

```bash
agentium tab send-message --submit --title Codex "$(cat <<'EOF'
Review /abs/path/to/file.md for <criteria>. Report only must-fix findings.

Reply in a single message with:
agentium tab send-message --submit --title Claude "<your reply>"
EOF
)"
```

- Pass the body inline as above. Never write it to a temp file and `cat` it back: sandboxed and unsandboxed commands resolve `$TMPDIR` to different directories, and you would send an empty message.
- One message per turn. Repeat absolute paths, the criteria, and the exact reply command in every message; the peer does not remember earlier ones.
- Exit 0 means the text was pasted and Enter was pressed in exactly one matching tab. It does not prove the peer started a turn; for a Claude Code peer, `agentium tab list` showing `running` does.
- Exit 1 with `no tab titled`, `N tabs titled`, `is not a terminal`, or `no arena` means nothing was sent. Fix `--title` or `--arena` with `agentium tab list`; do not retry the same command.
- `cannot reach Agentium` means the app is not running or predates the CLI socket.
- If a Claude Code peer shows `permission`, it is waiting at a dialog; your text could answer it. Ask the user instead of sending.

## Wait for the reply

End your turn after sending. The reply arrives as your next user message, prefixed `[from: Codex]` (the CLI adds the sender line for any message sent from an Agentium tab). Do not poll, sleep, or read the peer's screen. If the user reports no reply, confirm the title with `agentium tab list` and resend once.

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
