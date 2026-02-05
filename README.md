# Leonard

Leonard is an AI agent pair-programming orchestrator that coordinates two coding agents in a collaborative loop: a **Driver** (Claude Code) that writes code, and a **Navigator** (Codex) that reviews and provides guidance. Leonard runs each agent as a subprocess, parses their JSON output to extract text, and forwards it between them in a turn-based cycle.

## Quick Start (macOS)

For macOS users (if you don't have Homebrew, install it from [brew.sh](https://brew.sh)):

```bash
# Install rustup-init (Rust toolchain installer)
brew install rustup-init

# Run the installer to set up Rust (will offer to update your PATH)
rustup-init

# Load Rust into your PATH if needed (or open a new terminal)
source "$HOME/.cargo/env"

# Verify installation
rustc --version
cargo --version

# Clone and build Leonard
git clone https://github.com/rsdouglas/leonard
cd leonard
cargo build --release
```

You'll also need the `claude` and `codex` CLI tools (see [Prerequisites](#prerequisites) below).

## Architecture

Leonard implements a simple relay pattern for AI pair-programming:

1. **Driver** receives a task and produces code changes
2. **Navigator** reviews the Driver's output and provides feedback
3. **Driver** receives the feedback via `--continue` and iterates
4. Repeat until `--max-turns` reached

```
┌─────────────────────────────────────────────────────────────┐
│                        Leonard                              │
│                                                             │
│  Task → Driver (claude) → Navigator (codex) → Driver → ...  │
│         └─ writes code     └─ reviews/guides  └─ iterates   │
└─────────────────────────────────────────────────────────────┘
```

## Prerequisites

Leonard requires:

1. **Rust toolchain** (1.70+) - for building Leonard itself
   - **Recommended**: Install via `rustup-init` for version management:
     ```bash
     brew install rustup-init
     rustup-init
     source "$HOME/.cargo/env"  # or open a new terminal
     ```
   - **Alternative**: Install directly via Homebrew (simpler, but no toolchain management):
     ```bash
     brew install rust
     ```

2. **`claude` CLI** - Anthropic's Claude Code CLI tool
   - Used as the Driver agent (writes code)
   - Must support `-p`, `--continue`, `--permission-mode`, `--output-format stream-json`
   - Requires `ANTHROPIC_API_KEY` environment variable

3. **`codex` CLI** - OpenAI Codex CLI tool
   - Used as the Navigator agent (reviews and guides)
   - Must support `exec`, `--sandbox read-only`, `--json`
   - Requires `OPENAI_API_KEY` environment variable

## Installation

If you already completed the [Quick Start](#quick-start), you can skip the Rust installation steps.

```bash
# Clone the repository
git clone https://github.com/rsdouglas/leonard
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
| `--task <string>` | Initial task prompt for the Driver | (required) |
| `--max-turns <n>` | Maximum relay turns (0 = unlimited) | 10 |
| `--strip-ansi` | Strip ANSI escape codes from output | true |
| `--max-forward-bytes <n>` | Max bytes forwarded between agents | 100000 |
| `-c, --continue` | Resume previous Claude session | false |
| `--log-file <path>` | Log prompts and responses to file | (none) |

### Environment Variables

Required:
- `ANTHROPIC_API_KEY` - API key for Claude (Driver)
- `OPENAI_API_KEY` - API key for Codex (Navigator)

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

## Per-Repository Context (`leonard.md`)

Leonard automatically loads a `leonard.md` file from `--cwd` if provided, otherwise the current working directory. This file provides context and guidance to both agents about the specific repository they're working in.

Use `leonard.md` to document:
- Project-specific conventions and patterns
- Architecture decisions and constraints
- Testing requirements and procedures
- Build and deployment instructions
- Common gotchas or known issues
- Preferred libraries or approaches for the codebase

The contents of `leonard.md` are included in the initial prompts to both agents, giving them shared context about the project from the start.

**Example** `leonard.md`:
```markdown
# Project Context

This is a React + TypeScript project using Vite.

## Conventions
- Use functional components with hooks, not class components
- All API calls go through `src/lib/api.ts`
- Tests use Vitest and React Testing Library

## Before submitting code
- Run `npm test` to ensure tests pass
- Run `npm run typecheck` to verify TypeScript
```

## How It Works

Leonard runs preflight checks at startup to validate that `claude` and `codex` binaries are available and warn if API keys are missing.

1. **Driver turn**: Leonard spawns `claude -p` with the task, captures stdout and parses JSON events to extract text
2. **Navigator turn**: Extracted Driver text is forwarded to `codex exec --sandbox read-only` (first turn) or `codex resume --last` (continuation)
3. **Driver continuation**: Navigator feedback is parsed from JSONL and sent to `claude -p --continue`
4. **Repeat**: Steps 2-3 repeat until max-turns reached or interrupted

Output is streamed to stdout with section headers (`=== DRIVER ===`, `=== NAVIGATOR (turn N) ===`). Logs with timestamps go to stderr.

## Architecture Notes

Leonard spawns both agents as child processes and uses `stdout` pipes (`Stdio::piped()`) to capture their output. Stderr is also captured and displayed if a process exits with non-zero status.

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
