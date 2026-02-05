use anyhow::{Context, Result};
use clap::Parser;
use colored::{ColoredString, Colorize};
use serde::Deserialize;
use std::io::{IsTerminal, Write as _};
use std::path::PathBuf;
use std::process::Stdio;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// Claude stream-json event types
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
enum ClaudeEvent {
    #[serde(rename = "assistant")]
    Assistant { message: AssistantMessage },
    #[serde(rename = "user")]
    User { message: UserMessage },
    #[serde(rename = "result")]
    Result {
        #[allow(dead_code)]
        result: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
struct UserMessage {
    content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
enum ContentBlock {
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse { name: String },
    #[serde(rename = "tool_result")]
    ToolResult { content: Option<serde_json::Value> },
    #[serde(other)]
    Unknown,
}

/// Codex JSONL event types
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum CodexEvent {
    #[serde(rename = "item.completed")]
    ItemCompleted { item: CodexItem },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum CodexItem {
    #[serde(rename = "reasoning")]
    Reasoning { text: Option<String> },
    #[serde(rename = "agent_message")]
    AgentMessage { text: Option<String> },
    #[serde(rename = "command_execution")]
    CommandExecution {
        command: Option<String>,
        exit_code: Option<i32>,
        output: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Parser, Debug)]
#[command(name = "leonard")]
#[command(about = "Relay text between Driver and Navigator agents")]
struct Args {
    /// Working directory for both agents
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Overarching task to give the driver
    #[arg(long)]
    task: Option<String>,

    /// Maximum number of relay turns (0 = unlimited)
    #[arg(long, default_value_t = 10)]
    max_turns: usize,

    /// Strip ANSI escape codes from output
    #[arg(long, default_value_t = true)]
    strip_ansi: bool,

    /// Max bytes of output to forward between agents
    #[arg(long, default_value_t = 100_000)]
    max_forward_bytes: usize,

    /// Resume the previous Claude session (use --continue on first driver call)
    #[arg(long, short = 'c')]
    r#continue: bool,

    /// Log prompts and responses to a file for debugging
    #[arg(long)]
    log_file: Option<PathBuf>,
}

fn timestamp() -> String {
    OffsetDateTime::now_local()
        .unwrap_or_else(|_| OffsetDateTime::now_utc())
        .format(&Rfc3339)
        .unwrap_or_else(|_| "<time>".to_string())
}

fn log_line(tag: &str, msg: &str) {
    eprintln!("{} [{}] {}", timestamp(), tag, msg);
}


fn should_use_colors() -> bool {
    // Respect NO_COLOR environment variable
    if std::env::var("NO_COLOR").is_ok() {
        return false;
    }

    // Check for dumb terminal
    if let Ok(term) = std::env::var("TERM") {
        if term == "dumb" {
            return false;
        }
    }

    // Check if stdout is a TTY
    std::io::stdout().is_terminal()
}

fn maybe_color<S: Into<String>>(s: S, color_fn: impl Fn(String) -> ColoredString) -> String {
    let text = s.into();
    if should_use_colors() {
        color_fn(text).to_string()
    } else {
        text
    }
}

fn strip_ansi(input: &str) -> String {
    let bytes = strip_ansi_escapes::strip(input);
    String::from_utf8_lossy(&bytes).to_string()
}

fn truncate_line(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}...", truncated)
    }
}

fn truncate(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        text.to_string()
    } else {
        let target_start = text.len() - max_bytes;
        let start = text
            .char_indices()
            .map(|(i, _)| i)
            .find(|&i| i >= target_start)
            .unwrap_or(text.len());
        format!("[...truncated...]\n{}", &text[start..])
    }
}

fn navigator_signaled_done(output: &str) -> bool {
    let trimmed = output.trim();
    trimmed == "ALL_DONE" || trimmed.to_uppercase() == "ALL_DONE"
}

fn summarize_tool_result(content: &Option<serde_json::Value>) -> String {
    match content {
        None => "done".to_string(),
        Some(serde_json::Value::String(s)) => {
            let lines: Vec<&str> = s.lines().collect();
            if lines.len() <= 3 {
                truncate_line(s, 100)
            } else {
                format!("{} lines", lines.len())
            }
        }
        Some(serde_json::Value::Array(arr)) => {
            let mut text_parts = Vec::new();
            for item in arr {
                if let Some(obj) = item.as_object() {
                    if obj.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(text) = obj.get("text").and_then(|t| t.as_str()) {
                            text_parts.push(text);
                        }
                    }
                }
            }
            if !text_parts.is_empty() {
                let combined = text_parts.join(" ");
                let lines: Vec<&str> = combined.lines().collect();
                if lines.len() <= 3 {
                    truncate_line(&combined, 100)
                } else {
                    format!("{} lines", lines.len())
                }
            } else {
                format!("{} items", arr.len())
            }
        }
        Some(v) => truncate_line(&v.to_string(), 50),
    }
}

fn summarize_command_output(output: &Option<String>) -> String {
    match output {
        None => String::new(),
        Some(s) => {
            let lines: Vec<&str> = s.lines().collect();
            if lines.len() <= 3 {
                truncate_line(s, 100)
            } else {
                format!("{} lines", lines.len())
            }
        }
    }
}




/// Kill child process and wait for it to exit
async fn kill_child(child: &mut Child, name: &str) {
    log_line("system", &format!("killing {} process", name));
    let _ = child.kill().await;
}

/// Check if a binary exists and is executable on PATH
async fn check_binary_exists(binary: &str) -> Result<()> {
    Command::new(binary)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .with_context(|| format!("Binary '{}' not found on PATH or not executable", binary))?;
    Ok(())
}

/// Validate that the working directory exists and is accessible
fn validate_working_directory(cwd: &PathBuf) -> Result<()> {
    if !cwd.exists() {
        anyhow::bail!("Working directory does not exist: {}", cwd.display());
    }
    if !cwd.is_dir() {
        anyhow::bail!("Path is not a directory: {}", cwd.display());
    }
    Ok(())
}

/// Warn if an API key is missing or empty (non-blocking)
fn warn_if_missing_api_key(key_name: &str, agent_name: &str) {
    match std::env::var(key_name) {
        Ok(val) if !val.trim().is_empty() => {
            // Key is set and non-empty, all good
        }
        Ok(_) => {
            // Key is set but empty/whitespace
            log_line(
                "system",
                &format!("warning: {} is empty (required for {})", key_name, agent_name)
            );
        }
        Err(_) => {
            // Key is not set
            log_line(
                "system",
                &format!("warning: {} not set (required for {})", key_name, agent_name)
            );
        }
    }
}

/// Run all preflight checks before starting agent orchestration
async fn validate_prerequisites(args: &Args) -> Result<()> {
    // 1. Check binaries exist (lightweight --version check)
    check_binary_exists("claude")
        .await
        .context("Driver binary 'claude' not found. Install Claude Code CLI.")?;
    check_binary_exists("codex")
        .await
        .context("Navigator binary 'codex' not found. Install Codex CLI.")?;

    // 2. Validate cwd if provided
    if let Some(ref cwd) = args.cwd {
        validate_working_directory(cwd)
            .context("Invalid working directory")?;
    }

    // 3. Warn about missing API keys (non-blocking)
    warn_if_missing_api_key("ANTHROPIC_API_KEY", "claude driver");
    warn_if_missing_api_key("OPENAI_API_KEY", "codex navigator");

    log_line("system", "preflight checks passed");
    Ok(())
}

/// Process a single driver stdout line, updating collected output
fn process_driver_line(
    line: &str,
    collected: &mut Vec<String>,
    out: &mut std::io::Stdout,
) -> bool {
    if let Ok(event) = serde_json::from_str::<ClaudeEvent>(line) {
        match event {
            ClaudeEvent::Assistant { message } => {
                for block in message.content {
                    match block {
                        ContentBlock::Text { text } => {
                            println!("{}", maybe_color(text.clone(), |s| s.cyan()));
                            collected.push(text);
                        }
                        ContentBlock::ToolUse { name } => {
                            print!("{}", maybe_color(format!("  [{}] ", name), |s| s.bright_cyan()));
                            let _ = out.flush();
                        }
                        _ => {}
                    }
                }
            }
            ClaudeEvent::User { message } => {
                for block in message.content {
                    if let ContentBlock::ToolResult { content } = block {
                        let summary = summarize_tool_result(&content);
                        println!("{}", maybe_color(format!("  -> {}", summary), |s| s.cyan().dimmed()));
                        collected.push(format!("  -> {}", summary));
                    }
                }
            }
            ClaudeEvent::Result { .. } | ClaudeEvent::Unknown => {}
        }
        true
    } else {
        false
    }
}

/// Process a single navigator stdout line, updating collected output
fn process_navigator_line(
    line: &str,
    collected: &mut Vec<String>,
    out: &mut std::io::Stdout,
) -> bool {
    if let Ok(CodexEvent::ItemCompleted { item }) = serde_json::from_str::<CodexEvent>(line) {
        match item {
            CodexItem::Reasoning { text } => {
                if let Some(t) = text {
                    if !t.is_empty() {
                        for l in t.lines() {
                            println!("{}", maybe_color(format!("  thinking: {}", truncate_line(l, 80)), |s| s.magenta().dimmed()));
                        }
                    }
                }
            }
            CodexItem::AgentMessage { text } => {
                if let Some(t) = text {
                    if !t.is_empty() {
                        println!("{}", maybe_color(t.clone(), |s| s.magenta()));
                        collected.push(t);
                    }
                }
            }
            CodexItem::CommandExecution { command, exit_code, output } => {
                let cmd_str = command.unwrap_or_default();
                if !cmd_str.is_empty() {
                    let summary = summarize_command_output(&output);
                    let exit = exit_code.unwrap_or(0);
                    if summary.is_empty() {
                        println!("{}", maybe_color(format!("  [exit {}] {}", exit, truncate_line(&cmd_str, 60)), |s| s.bright_magenta()));
                    } else {
                        println!(
                            "{}",
                            maybe_color(
                                format!(
                                    "  [exit {}] {} -> {}",
                                    exit,
                                    truncate_line(&cmd_str, 40),
                                    truncate_line(&summary, 30)
                                ),
                                |s| s.bright_magenta()
                            )
                        );
                    }
                    let _ = out.flush();
                }
            }
            CodexItem::Unknown => {}
        }
        true
    } else {
        false
    }
}

/// Run Claude in print mode with JSON streaming and return its output
async fn run_driver(
    cwd: &Option<PathBuf>,
    prompt: &str,
    is_continuation: bool,
) -> Result<String> {
    if prompt.trim().is_empty() {
        anyhow::bail!("Cannot run driver with empty prompt");
    }

    let mut cmd = Command::new("claude");
    cmd.arg("-p");
    cmd.arg("--verbose");
    cmd.arg("--output-format").arg("stream-json");
    cmd.arg("--dangerously-skip-permissions");
    cmd.arg("--permission-mode").arg("acceptEdits");

    if is_continuation {
        cmd.arg("--continue");
    }

    cmd.arg(prompt);

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        cmd.env("ANTHROPIC_API_KEY", key);
    }

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let prompt_preview: String = prompt.chars().take(80).collect();
    log_line(
        "driver",
        &format!(
            "prompt: {}{}",
            prompt_preview,
            if prompt.chars().count() > 80 { "..." } else { "" }
        ),
    );

    let mut child = cmd.spawn().context("failed to spawn claude")?;
    let stdout = child.stdout.take().context("missing driver stdout")?;
    let stderr = child.stderr.take().context("missing driver stderr")?;
    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();

    let mut collected = Vec::new();
    let mut stderr_lines = Vec::new();
    let mut out = std::io::stdout();
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut child_status = None;

    loop {
        tokio::select! {
            biased;

            _ = tokio::signal::ctrl_c() => {
                kill_child(&mut child, "driver").await;
                anyhow::bail!("interrupted by user");
            }

            status = child.wait(), if child_status.is_none() => {
                child_status = Some(status.context("failed to wait for claude")?);
                // Process exited - break out and drain remaining buffered lines
                break;
            }

            line = stdout_reader.next_line(), if !stdout_done => {
                match line {
                    Ok(Some(line)) => {
                        if !process_driver_line(&line, &mut collected, &mut out) {
                            log_line("driver-err", &format!("failed to parse stdout line: {}", truncate_line(&line, 100)));
                        }
                    }
                    Ok(None) => stdout_done = true,
                    Err(e) => {
                        log_line("driver-err", &format!("stdout read error: {}", e));
                        stdout_done = true;
                    }
                }
            }

            line = stderr_reader.next_line(), if !stderr_done => {
                match line {
                    Ok(Some(line)) => {
                        stderr_lines.push(line);
                    }
                    Ok(None) => stderr_done = true,
                    Err(e) => {
                        log_line("driver-err", &format!("stderr read error: {}", e));
                        stderr_done = true;
                    }
                }
            }
        }
    }

    // Drain any remaining lines from stdout/stderr after process exits
    while let Ok(Some(line)) = stdout_reader.next_line().await {
        if !process_driver_line(&line, &mut collected, &mut out) {
            log_line("driver-err", &format!("failed to parse stdout line during drain: {}", truncate_line(&line, 100)));
        }
    }
    while let Ok(Some(line)) = stderr_reader.next_line().await {
        stderr_lines.push(line);
    }

    let status = child_status.expect("child_status should be set");

    if !status.success() {
        if !stderr_lines.is_empty() {
            log_line("driver-err", "stderr output:");
            for line in &stderr_lines {
                log_line("driver-err", line);
            }
        }

        anyhow::bail!("driver exited with status: {}", status);
    }

    Ok(collected.join("\n"))
}

/// Build the initial driver prompt from task and/or context
fn build_driver_prompt(task: Option<&str>, context: Option<&str>) -> String {
    let mut parts = Vec::new();

    // Add guidance for pair programming
    parts.push(String::from(
        "Explain your plan first, so your peer and navigator can help identify blindspots, then build it with your peer's feedback."
    ));

    if let Some(t) = task {
        parts.push(format!("## Task\n{}", t));
    }

    if let Some(c) = context {
        parts.push(format!("## Context\n{}", c));
    }

    parts.join("\n\n")
}

/// Build the navigator meta-prompt that frames the review context
fn build_navigator_prompt(task: Option<&str>, context: Option<&str>, driver_output: &str, is_continuation: bool) -> String {
    if is_continuation {
        format!(
            r#"The driver has responded:

---
{driver_output}
---

Review this response. If the task is complete, respond with "ALL_DONE".
"#,
            driver_output = driver_output
        )
    } else {
        let mut prompt = String::from(
            r#"ROLE: Helpful Peer
You are acting as a helpful peer. Your job is to evaluate the driver's work for the task below.
Do not offer to do things. Discuss, comment, and guide the driver.
Your job is not to block the driver, but to help them make progress and point out things they may have missed.
Progress is the goal, not perfection. We work iteratively, so we can improve incrementally.

"#
        );

        if let Some(t) = task {
            prompt.push_str(&format!("## Original Task\n{}\n\n", t));
        }

        if let Some(c) = context {
            prompt.push_str(&format!("## Context\n{}\n\n", c));
        }

        prompt.push_str(&format!(
            r#"## Driver's Output

---
{driver_output}
---

If the task is complete, you can end the conversation with "ALL_DONE".
"#,
            driver_output = driver_output
        ));

        prompt
    }
}

/// Run Codex exec with JSON mode and return its output (read-only sandbox)
async fn run_navigator(
    cwd: &Option<PathBuf>,
    prompt: &str,
    is_continuation: bool,
) -> Result<String> {
    if prompt.trim().is_empty() {
        anyhow::bail!("Cannot run navigator with empty prompt");
    }

    let mut cmd = Command::new("codex");
    cmd.arg("exec");

    cmd.arg("--skip-git-repo-check");
    
    if is_continuation {
        cmd.arg("resume");
        cmd.arg("--last");
        cmd.arg("--json");
        cmd.arg(prompt);
    } else {
        cmd.arg("--sandbox").arg("read-only");
        cmd.arg("--json");
        cmd.arg(prompt);
    }

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        cmd.env("OPENAI_API_KEY", key);
    }

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let prompt_preview: String = prompt.chars().take(80).collect();
    log_line(
        "navigator",
        &format!(
            "prompt: {}{}",
            prompt_preview,
            if prompt.chars().count() > 80 { "..." } else { "" }
        ),
    );

    let mut child = cmd.spawn().context("failed to spawn codex")?;
    let stdout = child.stdout.take().context("missing navigator stdout")?;
    let stderr = child.stderr.take().context("missing navigator stderr")?;
    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();

    let mut collected = Vec::new();
    let mut stderr_lines = Vec::new();
    let mut out = std::io::stdout();
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut child_status = None;

    loop {
        tokio::select! {
            biased;

            _ = tokio::signal::ctrl_c() => {
                kill_child(&mut child, "navigator").await;
                anyhow::bail!("interrupted by user");
            }

            status = child.wait(), if child_status.is_none() => {
                child_status = Some(status.context("failed to wait for codex")?);
                // Process exited - break out and drain remaining buffered lines
                break;
            }

            line = stdout_reader.next_line(), if !stdout_done => {
                match line {
                    Ok(Some(line)) => {
                        if !process_navigator_line(&line, &mut collected, &mut out) {
                            log_line("navigator-err", &format!("failed to parse stdout line: {}", truncate_line(&line, 100)));
                        }
                    }
                    Ok(None) => stdout_done = true,
                    Err(e) => {
                        log_line("navigator-err", &format!("stdout read error: {}", e));
                        stdout_done = true;
                    }
                }
            }

            line = stderr_reader.next_line(), if !stderr_done => {
                match line {
                    Ok(Some(line)) => {
                        stderr_lines.push(line);
                    }
                    Ok(None) => stderr_done = true,
                    Err(e) => {
                        log_line("navigator-err", &format!("stderr read error: {}", e));
                        stderr_done = true;
                    }
                }
            }
        }
    }

    // Drain any remaining lines from stdout/stderr after process exits
    while let Ok(Some(line)) = stdout_reader.next_line().await {
        if !process_navigator_line(&line, &mut collected, &mut out) {
            log_line("navigator-err", &format!("failed to parse stdout line during drain: {}", truncate_line(&line, 100)));
        }
    }
    while let Ok(Some(line)) = stderr_reader.next_line().await {
        stderr_lines.push(line);
    }

    let status = child_status.expect("child_status should be set");

    if !status.success() {
        if !stderr_lines.is_empty() {
            log_line("navigator-err", "stderr output:");
            for line in &stderr_lines {
                log_line("navigator-err", line);
            }
        }

        anyhow::bail!("navigator exited with status: {}", status);
    }

    Ok(collected.join("\n"))
}


async fn run_batch(args: &Args, task: Option<&str>, context: Option<&str>) -> Result<()> {
    if let Some(t) = task {
        log_line("system", &format!("task: {}", t));
    }
    if let Some(c) = context {
        log_line("system", &format!("context: {} chars", c.chars().count()));
    }

    let driver_prompt = build_driver_prompt(task, context);

    println!("{}", maybe_color("=== DRIVER ===", |s| s.cyan().bold()));
    let mut driver_output = run_driver(&args.cwd, &driver_prompt, args.r#continue).await?;
    println!();

    if args.strip_ansi {
        driver_output = strip_ansi(&driver_output);
    }

    log_line("driver-out", &format!("{} bytes", driver_output.len()));

    let mut turn = 0;

    loop {
        let navigator_is_continuation = turn > 0 || args.r#continue;

        let truncated_driver = truncate(&driver_output, args.max_forward_bytes);
        let navigator_prompt = build_navigator_prompt(task, context, &truncated_driver, navigator_is_continuation);

        println!("{}", maybe_color(format!("=== NAVIGATOR (turn {}) ===", turn), |s| s.magenta().bold()));
        let mut navigator_output = run_navigator(&args.cwd, &navigator_prompt, navigator_is_continuation).await?;
        println!();

        if args.strip_ansi {
            navigator_output = strip_ansi(&navigator_output);
        }

        log_line("navigator-out", &format!("{} bytes", navigator_output.len()));

        if navigator_signaled_done(&navigator_output) {
            log_line("system", "navigator signaled ALL_DONE; ending loop");
            break;
        }

        let feedback = truncate(&navigator_output, args.max_forward_bytes);

        println!("{}", maybe_color(format!("=== DRIVER (turn {}) ===", turn + 1), |s| s.cyan().bold()));
        driver_output = run_driver(&args.cwd, &feedback, true).await?;
        println!();

        if args.strip_ansi {
            driver_output = strip_ansi(&driver_output);
        }

        log_line("driver-out", &format!("{} bytes", driver_output.len()));

        turn += 1;

        if args.max_turns > 0 && turn >= args.max_turns {
            log_line("system", &format!("max_turns ({}) reached", args.max_turns));
            break;
        }
    }

    log_line("system", &format!("done after {} turn(s)", turn));

    Ok(())
}


#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Run preflight checks before starting orchestration
    validate_prerequisites(&args).await?;

    // Read leonard.md if present in cwd
    let leonard_path = if let Some(ref dir) = args.cwd {
        dir.join("leonard.md")
    } else {
        PathBuf::from("leonard.md")
    };

    let context = if leonard_path.exists() {
        match std::fs::read_to_string(&leonard_path) {
            Ok(content) if !content.trim().is_empty() => Some(content),
            Ok(_) => None, // Empty/whitespace-only
            Err(e) => {
                log_line("system", &format!("warning: failed to read leonard.md: {}", e));
                None
            }
        }
    } else {
        None
    };

    // Normalize empty/whitespace task to None
    let task = args.task.as_deref().and_then(|t| {
        let trimmed = t.trim();
        if trimmed.is_empty() { None } else { Some(trimmed) }
    });

    // Validate we have at least one input
    if task.is_none() && context.is_none() {
        anyhow::bail!("Either --task or leonard.md must be provided");
    }

    run_batch(&args, task, context.as_deref()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // truncate() tests
    #[test]
    fn test_truncate_short_text() {
        let text = "Hello, world!";
        let result = truncate(text, 100);
        assert_eq!(result, "Hello, world!");
    }

    #[test]
    fn test_truncate_exact_length() {
        let text = "Hello";
        let result = truncate(text, 5);
        assert_eq!(result, "Hello");
    }

    #[test]
    fn test_truncate_long_text() {
        let text = "Hello, world! This is a longer message that needs truncation.";
        let result = truncate(text, 20);

        assert!(result.starts_with("[...truncated...]"));
        assert!(result.len() <= "[...truncated...]\n".len() + 20);
        assert!(result.contains("truncation."));
    }

    #[test]
    fn test_truncate_utf8_boundary() {
        // Test with emoji and multi-byte UTF-8 characters
        let text = "Hello 👋 世界";
        let result = truncate(text, 10);

        // Should not panic and should produce valid UTF-8
        assert!(!result.is_empty());
        // The result should either be the full string or a truncated valid UTF-8 string
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[test]
    fn test_truncate_zero_max() {
        let text = "Hello, world!";
        let result = truncate(text, 0);

        // Should handle edge case gracefully
        assert!(result.starts_with("[...truncated...]"));
    }

    // truncate_line() tests
    #[test]
    fn test_truncate_line_short() {
        let text = "Short";
        let result = truncate_line(text, 10);
        assert_eq!(result, "Short");
    }

    #[test]
    fn test_truncate_line_exact() {
        let text = "Exactly10!";
        let result = truncate_line(text, 10);
        assert_eq!(result, "Exactly10!");
    }

    #[test]
    fn test_truncate_line_long() {
        let text = "This is a very long line that should be truncated";
        let result = truncate_line(text, 20);
        assert_eq!(result, "This is a very long ...");
        assert_eq!(result.chars().count(), 23); // 20 chars + "..."
    }

    #[test]
    fn test_truncate_line_with_emoji() {
        let text = "Hello 👋👋👋👋👋👋👋";
        let result = truncate_line(text, 10);

        // Should count characters, not bytes
        assert!(result.chars().count() <= 13); // 10 + "..."
        assert!(result.ends_with("..."));
    }

    // strip_ansi() tests
    #[test]
    fn test_strip_ansi_no_codes() {
        let input = "Plain text";
        let result = strip_ansi(input);
        assert_eq!(result, "Plain text");
    }

    #[test]
    fn test_strip_ansi_with_color_codes() {
        let input = "\x1b[31mRed text\x1b[0m";
        let result = strip_ansi(input);
        assert_eq!(result, "Red text");
    }

    #[test]
    fn test_strip_ansi_multiple_codes() {
        let input = "\x1b[1m\x1b[32mBold green\x1b[0m normal \x1b[33myellow\x1b[0m";
        let result = strip_ansi(input);
        assert_eq!(result, "Bold green normal yellow");
    }

    #[test]
    fn test_strip_ansi_empty() {
        let input = "";
        let result = strip_ansi(input);
        assert_eq!(result, "");
    }

    // navigator_signaled_done() tests
    #[test]
    fn test_navigator_signaled_done_exact() {
        assert!(navigator_signaled_done("ALL_DONE"));
    }

    #[test]
    fn test_navigator_signaled_done_lowercase() {
        assert!(navigator_signaled_done("all_done"));
    }

    #[test]
    fn test_navigator_signaled_done_mixed_case() {
        assert!(navigator_signaled_done("All_Done"));
        assert!(navigator_signaled_done("aLL_dONE"));
    }

    #[test]
    fn test_navigator_signaled_done_with_whitespace() {
        assert!(navigator_signaled_done("  ALL_DONE  "));
        assert!(navigator_signaled_done("\nALL_DONE\n"));
        assert!(navigator_signaled_done("\t\tALL_DONE\t\t"));
    }

    #[test]
    fn test_navigator_signaled_done_false() {
        assert!(!navigator_signaled_done("Not done yet"));
        assert!(!navigator_signaled_done("ALMOST_DONE"));
        assert!(!navigator_signaled_done("ALL_DONE but more text"));
        assert!(!navigator_signaled_done(""));
    }

    // summarize_tool_result() tests
    #[test]
    fn test_summarize_tool_result_none() {
        let result = summarize_tool_result(&None);
        assert_eq!(result, "done");
    }

    #[test]
    fn test_summarize_tool_result_short_string() {
        let content = Some(json!("Short message"));
        let result = summarize_tool_result(&content);
        assert_eq!(result, "Short message");
    }

    #[test]
    fn test_summarize_tool_result_long_string() {
        let long_text = "x".repeat(150);
        let content = Some(json!(long_text));
        let result = summarize_tool_result(&content);

        assert!(result.len() <= 103); // 100 + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_summarize_tool_result_multiline_short() {
        let content = Some(json!("Line 1\nLine 2\nLine 3"));
        let result = summarize_tool_result(&content);

        // 3 lines or fewer should show the content
        assert!(result.contains("Line"));
    }

    #[test]
    fn test_summarize_tool_result_multiline_long() {
        let content = Some(json!("Line 1\nLine 2\nLine 3\nLine 4\nLine 5"));
        let result = summarize_tool_result(&content);

        // More than 3 lines should just show count
        assert_eq!(result, "5 lines");
    }

    #[test]
    fn test_summarize_tool_result_array_with_text() {
        let content = Some(json!([
            {"type": "text", "text": "First message"},
            {"type": "text", "text": "Second message"}
        ]));
        let result = summarize_tool_result(&content);

        assert!(result.contains("First message"));
    }

    #[test]
    fn test_summarize_tool_result_array_without_text() {
        let content = Some(json!([
            {"type": "image", "data": "..."},
            {"type": "other", "value": 123}
        ]));
        let result = summarize_tool_result(&content);

        assert_eq!(result, "2 items");
    }

    #[test]
    fn test_summarize_tool_result_other_json() {
        let content = Some(json!({"status": "ok", "count": 42}));
        let result = summarize_tool_result(&content);

        assert!(result.len() <= 50);
    }

    // summarize_command_output() tests
    #[test]
    fn test_summarize_command_output_none() {
        let result = summarize_command_output(&None);
        assert_eq!(result, "");
    }

    #[test]
    fn test_summarize_command_output_empty() {
        let result = summarize_command_output(&Some(String::new()));
        assert_eq!(result, "");
    }

    #[test]
    fn test_summarize_command_output_short() {
        let output = Some("Command output".to_string());
        let result = summarize_command_output(&output);
        assert_eq!(result, "Command output");
    }

    #[test]
    fn test_summarize_command_output_multiline_short() {
        let output = Some("Line 1\nLine 2\nLine 3".to_string());
        let result = summarize_command_output(&output);

        // 3 lines or fewer should show content
        assert!(result.contains("Line"));
    }

    #[test]
    fn test_summarize_command_output_multiline_long() {
        let output = Some("Line 1\nLine 2\nLine 3\nLine 4\nLine 5".to_string());
        let result = summarize_command_output(&output);

        assert_eq!(result, "5 lines");
    }

    #[test]
    fn test_summarize_command_output_long_single_line() {
        let long_output = Some("x".repeat(150));
        let result = summarize_command_output(&long_output);

        assert!(result.len() <= 103); // 100 + "..."
        assert!(result.ends_with("..."));
    }
}
