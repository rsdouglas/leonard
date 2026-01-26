# Leonard Architecture (Rust)

## Current Implementation (v0.1.0)

Leonard implements a simple turn-based relay using standard pipes (`Stdio::piped()`), not PTYs. This section documents the current architecture.

### Goals
- Run Maker and Critic as subprocesses with simple stdout/stderr capture.
- Forward Maker output verbatim to Critic.
- Forward Critic feedback back to Maker via `--continue` flag.
- Maintain turn-based synchronous execution until max-turns reached.

### Process Model
- Leonard is a single supervisor process.
- It spawns child processes sequentially (not concurrently):
  - `maker` (claude CLI with `-p` print mode)
  - `critic` (codex CLI with `exec` one-shot mode)
- Leonard uses `Stdio::piped()` to capture stdout/stderr from both.
- Communication is unidirectional: Leonard reads from stdout, does not write to stdin during execution.
- The `--continue` flag on Claude maintains conversation state across turns.

---

## Original Design (PTY-based - Future Work)

The following describes a more sophisticated PTY-based architecture that was initially planned. This has not been implemented yet but may be pursued in future versions for better terminal interaction and readiness detection.

### Original Goals
- Own both PTYs by spawning a Maker and a Critic as child processes.
- Stream Maker output, detect readiness, forward recent output to Critic.
- Inject Critic reply back into Maker input.
- Keep minimal state and be resilient to transient errors.

### Original Process Model
- Leonard is a single supervisor process.
- It spawns two child processes under PTYs:
  - `maker` (configurable CLI, e.g. Claude Code)
  - `critic` (configurable CLI, e.g. Codex)
- Leonard holds each PTY master and:
  - Reads output from both children.
  - Writes input to both children.

## Current Core Components (v0.1.0)

### 1) Command Execution
- Uses `tokio::process::Command` with `Stdio::piped()` for stdout/stderr.
- Spawns `claude -p` and `codex exec` sequentially, waiting for completion.
- Parses JSON/JSONL output from both CLIs to extract text content.

### 2) Output Collection
- Reads line-by-line from stdout using `tokio::io::BufReader`.
- Parses stream-json events from Claude, extracts text blocks.
- Parses JSONL events from Codex, extracts reasoning and agent messages.
- Collects all text into a single string for forwarding.

### 3) Truncation
- If collected output exceeds `max_forward_bytes`, keeps the end and prepends `[...truncated...]`.
- ANSI stripping is optional via `--strip-ansi` flag.

### 4) Logger
- Timestamped logs to stderr with tags: `[system]`, `[maker]`, `[critic]`.
- Section headers to stdout: `=== MAKER ===`, `=== CRITIC (turn N) ===`.

### 5) Ctrl+C Handling
- Uses `tokio::signal::ctrl_c()` with `tokio::select!` for graceful shutdown.
- Kills child processes on interrupt.

## Current Data Flow
1. Run Maker with task → capture stdout → parse JSON events → collect text.
2. Forward collected text to Critic → capture stdout → parse JSONL → collect text.
3. Forward Critic text to Maker with `--continue` → repeat from step 2.
4. Repeat until `max_turns` reached.

## Concurrency Model (v0.1.0)
- `tokio` async runtime with `#[tokio::main]`.
- Sequential subprocess execution (wait for completion before next spawn).
- Async line reading with `tokio::select!` for Ctrl+C handling.

---

## Original Design Components (PTY-based - Not Implemented)

### 1) Session Manager
- Spawns child processes under PTYs.
- Tracks `ChildHandle` (pid, pty master, stdin writer, stdout reader).
- Handles restarts if a child exits unexpectedly (optional in v1).

### 2) Maker Output Buffer
- Maintains an append-only buffer of Maker output.
- Tracks `last_forwarded_offset`.
- Provides `get_new_since_last()` for forwarding.
- Optional: strip ANSI escape codes before forwarding.

### 3) Readiness Detector
- Watches Maker output for steady state (no output change).
- Uses a configurable idle window (`maker_idle_ms`).
- Debounces detection to avoid duplicate sends.

### 4) Forwarder
- On readiness:
  - Fetches Maker output since `last_forwarded_offset`.
  - Sends it to Critic as a single prompt.
  - Awaits Critic response completion (simple heuristic: prompt marker or idle timeout).
  - Sends Critic response to Maker input.
  - Advances `last_forwarded_offset`.

### 5) Critic Response Collector
- Reads Critic output and detects response completion.
- Uses a configurable regex / prompt marker or "idle gap" timeout.

### 6) Logger
- Structured logs with timestamps and event types.
- Optional debug logging of forwarded text (guarded by a flag).

## Original Data Flow
1. Maker produces output → Maker buffer.
2. Readiness detector sees prompt marker.
3. Forwarder sends Maker delta → Critic.
4. Critic outputs response → collector.
5. Forwarder injects response → Maker.

## Original Concurrency Model
- `tokio` runtime.
- Separate tasks:
  - `claude_reader` → buffer + readiness detector.
  - `codex_reader` → response collector.
  - `forwarder` → reacts to readiness events.
- Use channels (`tokio::sync::mpsc`) for events and text chunks.

## Current Configuration (v0.1.0)
CLI flags in main.rs:78-109:
- `--cwd`: working directory for both agents.
- `--task`: initial task prompt for Maker.
- `--max-turns`: maximum relay turns (default: 10).
- `--strip-ansi`: strip ANSI escape codes (default: true).
- `--max-forward-bytes`: max bytes to forward (default: 100000).
- `--continue`: resume previous Claude session.
- `--log-file`: log prompts and responses to a file for debugging.

Environment variables:
- `ANTHROPIC_API_KEY`: passed to `claude`.
- `OPENAI_API_KEY`: passed to `codex`.

Hardcoded:
- `maker_cmd`: `claude` (main.rs:205).
- `critic_cmd`: `codex` (main.rs:358).

## Current Error Handling (v0.1.0)
- Subprocess spawn failures: return error, exit immediately.
- JSON parse failures: skip unparseable lines, continue.
- Ctrl+C: kill child processes, exit gracefully.
- Child exit: wait for completion, check status code.

## Security & Safety
- Leonard only forwards plain text between two local CLIs.
- No external network calls beyond what Claude/Codex already do.

---

## Original Design Configuration (PTY-based)
- `maker_cmd`: executable + args.
- `critic_cmd`: executable + args.
- `cwd`: working directory.
- `maker_idle_ms` for steady-state detection.
- `critic_profile` with built-in readiness defaults (Claude/Codex), override via `critic_ready_regex`.
- `critic_idle_ms` as fallback completion detector.
- `max_forward_bytes`.
- `strip_ansi`.

## Original Error Handling
- If Claude readiness is ambiguous, ignore.
- If Codex errors, retry once; otherwise pause forwarding.
- If either child exits, log and stop (v1) or restart (v2).

## Original Milestones
1. Spawn both under PTY; echo streams to stdout for visibility.
2. Implement buffer + readiness detection for Claude.
3. Implement Codex prompt/response collection.
4. Wire forwarder loop with debouncing.
5. Add minimal config + logging.

---

## Why PTY-based Design Was Not Implemented

The original architecture document described a PTY-based design for better interactive terminal control and readiness detection. The current implementation uses simpler `Stdio::piped()` approach because:

1. **Simplicity**: Pipes are easier to work with and require less platform-specific code.
2. **JSON Output**: Both `claude` and `codex` support JSON output modes, eliminating need for terminal parsing.
3. **Conversation State**: Claude's `--continue` flag maintains state across invocations, removing need for long-running PTY sessions.
4. **Synchronous Model**: The turn-based pattern fits naturally with sequential subprocess execution.

PTY support may be added in the future if interactive features or real-time streaming become requirements.
