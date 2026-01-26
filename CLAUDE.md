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
- Uses `codex exec --read-only` for critic
- Verbatim forwarding - no role prompts or framing added
- `--continue` flag maintains Claude conversation state across turns

## UTF-8 String Handling

Rust strings are UTF-8 encoded, meaning characters can be 1-4 bytes. Common gotchas:

**NEVER slice strings at arbitrary byte positions:**
```rust
// BAD: panics if byte 80 is in the middle of a multi-byte char
&text[..80]

// GOOD: use char iterators
text.chars().take(80).collect::<String>()
```

**Cursor positions should be character indices, not byte indices:**
```rust
// BAD: cursor is byte position, insert expects byte index
edit_buffer.insert(cursor, c);
cursor += 1;  // Wrong! Multi-byte chars need cursor += c.len_utf8()

// GOOD: cursor is char index, convert to byte index when needed
let byte_idx = s.char_indices().nth(char_idx).map(|(i,_)| i).unwrap_or(s.len());
edit_buffer.insert(byte_idx, c);
cursor += 1;  // Correct as char index
```

**Use `.chars().count()` not `.len()` for character counts.**

## Environment

Requires `.envrc` with:
- `ANTHROPIC_API_KEY` for Claude
- `OPENAI_API_KEY` for Codex (if using OpenAI backend)
