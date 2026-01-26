# Leonard

Leonard is an AI agent pair-programming orchestrator that coordinates two coding agents in a collaborative loop: a **Maker** (Claude Code) that writes code, and an **Assistant** (Codex) that reviews and provides guidance. Leonard runs each agent as a subprocess, parses their JSON output to extract text, and forwards it between them in a turn-based cycle.

## Architecture

Leonard implements a simple relay pattern for AI pair-programming:

1. **Maker** receives a task and produces code changes
2. **Assistant** reviews the Maker's output and provides feedback
3. **Maker** receives the feedback via `--continue` and iterates
4. Repeat until `--max-turns` reached

```
┌─────────────────────────────────────────────────────────────┐
│                        Leonard                              │
│                                                             │
│  Task → Maker (claude) → Assistant (codex) → Maker → ...   │
│         └─ writes code    └─ reviews/guides   └─ iterates   │
└─────────────────────────────────────────────────────────────┘
```

## Prerequisites

Leonard requires these external CLI tools in your `PATH`:

1. **`claude` CLI** - Anthropic's Claude Code CLI tool
   - Used as the Maker agent (writes code)
   - Must support `-p`, `--continue`, `--permission-mode`, `--output-format stream-json`
   - Requires `ANTHROPIC_API_KEY` environment variable

2. **`codex` CLI** - OpenAI Codex CLI tool
   - Used as the Assistant agent (reviews and guides)
   - Must support `exec`, `--sandbox read-only`, `--json`
   - Requires `OPENAI_API_KEY` environment variable

3. **Rust toolchain** (1.70+) - for building Leonard itself

## Installation

```bash
# Clone the repository
git clone <repository-url>
cd leonard

# Copy environment template and add your API keys
cp .env.example .envrc
# Edit .envrc with your actual keys

# Build the binary
cargo build --release

# Binary will be at target/release/leonard
```

## Usage

Basic usage:

```bash
leonard --cwd /path/to/repo --task "Add error handling to the login function" --max-turns 5
```

### CLI Options

| Flag | Description | Default |
|------|-------------|---------|
| `--cwd <path>` | Working directory for both agents | current directory |
| `--task <string>` | Initial task prompt for the Maker | (required) |
| `--max-turns <n>` | Maximum relay turns (0 = unlimited) | 10 |
| `--strip-ansi` | Strip ANSI escape codes from output | true |
| `--max-forward-bytes <n>` | Max bytes forwarded between agents | 100000 |
| `-c, --continue` | Resume previous Claude session | false |
| `--log-file <path>` | Log prompts and responses to file | (none) |

### Environment Variables

Required:
- `ANTHROPIC_API_KEY` - API key for Claude (Maker)
- `OPENAI_API_KEY` - API key for Codex (Critic)

Optional:
- Use `.envrc` with [direnv](https://direnv.net/) for automatic loading
- Or export manually: `export ANTHROPIC_API_KEY=...`

### Example

Run Leonard on a codebase with a specific task:

```bash
# Set environment variables
source .envrc

# Run a 3-turn pair-programming loop
cargo run -- \
  --cwd /path/to/your/project \
  --task "Refactor the database connection code to use connection pooling" \
  --max-turns 3
```

For testing without waiting, use background execution (see CLAUDE.md).

## How It Works

1. **Maker turn**: Leonard spawns `claude -p` with the task, captures stdout and parses JSON events to extract text
2. **Assistant turn**: Extracted Maker text is forwarded to `codex exec --sandbox read-only` (first turn) or `codex resume --last` (continuation)
3. **Maker continuation**: Assistant feedback is parsed from JSONL and sent to `claude -p --continue`
4. **Repeat**: Steps 2-3 repeat until max-turns reached or interrupted

Output is streamed to stdout with section headers (`=== MAKER ===`, `=== CRITIC (turn N) ===`). Logs with timestamps go to stderr.

**Note on terminology**: Leonard conceptually frames the second agent as an "Assistant" (reflecting the pair-programming guidance role), but the CLI output and code internals use "CRITIC" as the technical term.

## Architecture Notes

The current implementation uses simple `stdout` pipes (`Stdio::piped()`), not PTYs. Leonard only reads stdout from child processes; stderr is piped but not consumed. The `architecture.md` file originally described a PTY-based design, which may be implemented in the future for better terminal interaction, but the current release uses the simpler pipe-based approach.

## Contributing

Contributions welcome. Before opening a PR:

1. Run `cargo test` to ensure tests pass
2. Run `cargo clippy` to check for lint warnings
3. Run `cargo fmt` to format code

## License

MIT License - see LICENSE file for details.

## Notes

- **CLI Tool Availability**: The `claude` and `codex` CLI tools are currently required dependencies. Configuration options to override these may be added in the future.
- **Text Extraction**: Leonard parses JSON/JSONL output from both agents to extract text content, then forwards the extracted text between them.
- **Truncation**: If output exceeds `--max-forward-bytes`, the end of the text is kept with a `[...truncated...]` prefix.
