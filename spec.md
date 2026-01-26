# Leonard Spec

## Summary
Leonard is a small supervisor that relays text between two agents: a **Maker** (Claude Code) and a **Critic** (Codex). It runs each agent as a subprocess in print-mode, captures output, and forwards it verbatim to the other agent in a turn-based loop until `max_turns` is reached.

## Goals
- Relay Maker output to Critic for review.
- Relay Critic feedback back to Maker for continuation.
- Simple, synchronous turn-based loop.

## Non-goals
- No semantic analysis or task planning.
- No PTY control or idle-timeout detection.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                        Leonard                              │
│                                                             │
│  1. Run Maker with task prompt                              │
│          │                                                  │
│          ▼                                                  │
│  ┌───────────────┐                                          │
│  │ claude -p ... │ ──► stdout captured                      │
│  └───────────────┘                                          │
│          │                                                  │
│          ▼                                                  │
│  2. Forward output verbatim to Critic                       │
│          │                                                  │
│          ▼                                                  │
│  ┌───────────────────┐                                      │
│  │ codex exec ...    │ ──► stdout captured                  │
│  └───────────────────┘                                      │
│          │                                                  │
│          ▼                                                  │
│  3. Forward Critic output verbatim to Maker (--continue)    │
│          │                                                  │
│          ▼                                                  │
│  ┌─────────────────────┐                                    │
│  │ claude -p --continue │ ──► repeat from step 2            │
│  └─────────────────────┘                                    │
│          │                                                  │
│          ▼                                                  │
│  4. Repeat until max_turns reached                          │
└─────────────────────────────────────────────────────────────┘
```

## Agent Commands

**Maker** (Claude Code in print mode):
```
claude -p --permission-mode acceptEdits [--continue] "<prompt>"
```
- `-p`: print mode (non-interactive, outputs to stdout)
- `--permission-mode acceptEdits`: auto-accept file edits
- `--continue`: resume previous conversation (after first turn)
- `TERM=xterm-256color` is set in the environment

**Critic** (Codex exec):
```
codex exec --read-only [-C <dir>] "<prompt>"
```
- `exec`: one-shot execution mode
- `--read-only`: allow reads but block writes
- `-C <dir>`: passed when `--cwd` is specified
- `TERM=xterm-256color` is set in the environment

## CLI Options

| Option | Default | Description |
|--------|---------|-------------|
| `--cwd <path>` | current dir | Working directory for both agents |
| `--task <string>` | required | Task prompt for the Maker |
| `--max-turns <n>` | 10 | Maximum relay turns (0 = unlimited) |
| `--strip-ansi` | true | Strip ANSI escape codes from output |
| `--max-forward-bytes <n>` | 100000 | Max bytes of output to forward between agents |

## Relay Loop

1. **Initial turn**: Run Maker with task prompt (no `--continue`).
2. **Forward to Critic**: Send Maker output verbatim to Critic (truncated if needed).
3. **Forward to Maker**: Send Critic output verbatim to Maker with `--continue` (truncated if needed).
4. **Repeat** from step 2 until `max_turns` reached.

## Truncation
When output exceeds `max_forward_bytes`, truncation keeps the **end** of the text and prepends `[...truncated...]`.

## Logging
- All logs go to stderr with RFC3339 timestamps.
- Tags: `[system]`, `[maker]`, `[maker-err]`, `[maker-out]`, `[critic]`, `[critic-err]`, `[critic-out]`.
- Maker/Critic output also printed to stdout with section headers (`=== MAKER ===`, `=== CRITIC (turn N) ===`).

## Environment Variables
- `ANTHROPIC_API_KEY`: passed to Claude.
- `OPENAI_API_KEY`: passed to Codex.
