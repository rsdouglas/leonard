# Claude Code Notes for Leonard

## Testing Leonard

Don't use long timeouts waiting for completion. Instead:

```bash
# Run in background, output to file
source .envrc && cargo run -- \
  --cwd /path/to/repo \
  --task "your task here" \
  --max-turns 3 \
  > /tmp/leonard-test.out 2>&1 &

# Tail the output
tail -f /tmp/leonard-test.out

# Or check periodically
cat /tmp/leonard-test.out | tail -50
```

## Key Implementation Details

- Uses `claude -p --permission-mode acceptEdits` for maker (allows file writes)
- Uses `codex exec --full-auto` for critic
- Verbatim forwarding - no role prompts or framing added
- `--continue` flag maintains Claude conversation state across turns

## Environment

Requires `.envrc` with:
- `ANTHROPIC_API_KEY` for Claude
- `OPENAI_API_KEY` for Codex (if using OpenAI backend)
