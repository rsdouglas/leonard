# Leonard Architecture (Rust)

## Goals
- Own both PTYs by spawning a Maker and a Critic as child processes.
- Stream Maker output, detect readiness, forward recent output to Critic.
- Inject Critic reply back into Maker input.
- Keep minimal state and be resilient to transient errors.

## Process Model
- Leonard is a single supervisor process.
- It spawns two child processes under PTYs:
  - `maker` (configurable CLI, e.g. Claude Code)
  - `critic` (configurable CLI, e.g. Codex)
- Leonard holds each PTY master and:
  - Reads output from both children.
  - Writes input to both children.

## Core Components

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
- Uses a configurable regex / prompt marker or “idle gap” timeout.

### 6) Logger
- Structured logs with timestamps and event types.
- Optional debug logging of forwarded text (guarded by a flag).

## Data Flow
1. Maker produces output → Maker buffer.
2. Readiness detector sees prompt marker.
3. Forwarder sends Maker delta → Critic.
4. Critic outputs response → collector.
5. Forwarder injects response → Maker.

## Concurrency Model
- `tokio` runtime.
- Separate tasks:
  - `claude_reader` → buffer + readiness detector.
  - `codex_reader` → response collector.
  - `forwarder` → reacts to readiness events.
- Use channels (`tokio::sync::mpsc`) for events and text chunks.

## Configuration
- `maker_cmd`: executable + args.
- `critic_cmd`: executable + args.
- `cwd`: working directory (Triaster repo).
- `maker_idle_ms` for steady-state detection.
- `critic_profile` with built-in readiness defaults (Claude/Codex), override via `critic_ready_regex`.
- `critic_idle_ms` as fallback completion detector.
- `max_forward_bytes`.
- `strip_ansi`.

## Error Handling
- If Claude readiness is ambiguous, ignore.
- If Codex errors, retry once; otherwise pause forwarding.
- If either child exits, log and stop (v1) or restart (v2).

## Security & Safety
- Leonard only forwards plain text between two local CLIs.
- No external network calls beyond what Claude/Codex already do.

## Milestones
1. Spawn both under PTY; echo streams to stdout for visibility.
2. Implement buffer + readiness detection for Claude.
3. Implement Codex prompt/response collection.
4. Wire forwarder loop with debouncing.
5. Add minimal config + logging.
