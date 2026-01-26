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
#[command(about = "Relay text between Maker and Critic agents")]
struct Args {
    /// Working directory for both agents
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Overarching task to give the maker
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

    /// Resume the previous Claude session (use --continue on first maker call)
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

pub fn strip_ansi(input: &str) -> String {
    let bytes = strip_ansi_escapes::strip(input);
    String::from_utf8_lossy(&bytes).to_string()
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

fn truncate_line(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}...", truncated)
    }
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

/// Run Claude in print mode with JSON streaming and return its output
async fn run_maker(
    cwd: &Option<PathBuf>,
    prompt: &str,
    is_continuation: bool,
) -> Result<String> {
    if prompt.trim().is_empty() {
        anyhow::bail!("Cannot run maker with empty prompt");
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
        "maker",
        &format!(
            "prompt: {}{}",
            prompt_preview,
            if prompt.chars().count() > 80 { "..." } else { "" }
        ),
    );

    let mut child = cmd.spawn().context("failed to spawn claude")?;
    let stdout = child.stdout.take().context("missing maker stdout")?;
    let mut reader = BufReader::new(stdout).lines();

    let mut collected = Vec::new();
    let mut out = std::io::stdout();

    loop {
        tokio::select! {
            biased;

            _ = tokio::signal::ctrl_c() => {
                kill_child(&mut child, "maker").await;
                anyhow::bail!("interrupted by user");
            }

            line = reader.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if let Ok(event) = serde_json::from_str::<ClaudeEvent>(&line) {
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
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        log_line("maker-err", &format!("read error: {}", e));
                        break;
                    }
                }
            }
        }
    }

    let status = child.wait().await.context("failed to wait for claude")?;

    if !status.success() {
        anyhow::bail!("maker exited with status: {}", status);
    }

    Ok(collected.join("\n"))
}

/// Build the initial maker prompt from task and/or context
fn build_maker_prompt(task: Option<&str>, context: Option<&str>) -> String {
    let mut parts = Vec::new();

    if let Some(t) = task {
        parts.push(format!("## Task\n{}", t));
    }

    if let Some(c) = context {
        parts.push(format!("## Context\n{}", c));
    }

    parts.join("\n\n")
}

/// Build the critic meta-prompt that frames the review context
fn build_critic_prompt(task: Option<&str>, context: Option<&str>, maker_output: &str, is_continuation: bool) -> String {
    if is_continuation {
        format!(
            r#"The maker has responded:

---
{maker_output}
---

Review this response. If the task is complete, respond with "ALL_DONE".
"#,
            maker_output = maker_output
        )
    } else {
        let mut prompt = String::from(
            r#"ROLE: Helpful Peer
You are acting as a helpful peer. Your job is to evaluate the maker's work for the task below.
Do not offer to do things. Discuss, comment, and guide the maker.
Your job is not to block the maker, but to help them make progress and point out things they may have missed.
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
            r#"## Maker's Output

---
{maker_output}
---

If the task is complete, you can end the conversation with "ALL_DONE".
"#,
            maker_output = maker_output
        ));

        prompt
    }
}

/// Run Codex exec with JSON mode and return its output (read-only sandbox)
async fn run_critic(
    cwd: &Option<PathBuf>,
    prompt: &str,
    is_continuation: bool,
) -> Result<String> {
    if prompt.trim().is_empty() {
        anyhow::bail!("Cannot run critic with empty prompt");
    }

    let mut cmd = Command::new("codex");
    cmd.arg("exec");

    if is_continuation {
        cmd.arg("resume");
        cmd.arg("--last");
        cmd.arg("--json");
        cmd.arg(prompt);
    } else {
        cmd.arg("--sandbox").arg("read-only");
        cmd.arg("--json");
        if let Some(dir) = cwd {
            cmd.arg("-C").arg(dir);
        }
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
        "critic",
        &format!(
            "prompt: {}{}",
            prompt_preview,
            if prompt.chars().count() > 80 { "..." } else { "" }
        ),
    );

    let mut child = cmd.spawn().context("failed to spawn codex")?;
    let stdout = child.stdout.take().context("missing critic stdout")?;
    let mut reader = BufReader::new(stdout).lines();

    let mut collected = Vec::new();
    let mut out = std::io::stdout();

    loop {
        tokio::select! {
            biased;

            _ = tokio::signal::ctrl_c() => {
                kill_child(&mut child, "critic").await;
                anyhow::bail!("interrupted by user");
            }

            line = reader.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if let Ok(event) = serde_json::from_str::<CodexEvent>(&line) {
                            if let CodexEvent::ItemCompleted { item } = event {
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
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        log_line("critic-err", &format!("read error: {}", e));
                        break;
                    }
                }
            }
        }
    }

    let status = child.wait().await.context("failed to wait for codex")?;

    if !status.success() {
        anyhow::bail!("critic exited with status: {}", status);
    }

    Ok(collected.join("\n"))
}

pub fn truncate(text: &str, max_bytes: usize) -> String {
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

async fn run_batch(args: &Args, task: Option<&str>, context: Option<&str>) -> Result<()> {
    if let Some(t) = task {
        log_line("system", &format!("task: {}", t));
    }
    if let Some(c) = context {
        log_line("system", &format!("context: {} chars", c.chars().count()));
    }

    let maker_prompt = build_maker_prompt(task, context);

    println!("{}", maybe_color("=== MAKER ===", |s| s.cyan().bold()));
    let mut maker_output = run_maker(&args.cwd, &maker_prompt, args.r#continue).await?;
    println!();

    if args.strip_ansi {
        maker_output = strip_ansi(&maker_output);
    }

    log_line("maker-out", &format!("{} bytes", maker_output.len()));

    let mut turn = 0;

    loop {
        let critic_is_continuation = turn > 0 || args.r#continue;

        let truncated_maker = truncate(&maker_output, args.max_forward_bytes);
        let critic_prompt = build_critic_prompt(task, context, &truncated_maker, critic_is_continuation);

        println!("{}", maybe_color(format!("=== CRITIC (turn {}) ===", turn), |s| s.magenta().bold()));
        let mut critic_output = run_critic(&args.cwd, &critic_prompt, critic_is_continuation).await?;
        println!();

        if args.strip_ansi {
            critic_output = strip_ansi(&critic_output);
        }

        log_line("critic-out", &format!("{} bytes", critic_output.len()));

        if critic_signaled_done(&critic_output) {
            log_line("system", "critic signaled ALL_DONE; ending loop");
            break;
        }

        let feedback = truncate(&critic_output, args.max_forward_bytes);

        println!("{}", maybe_color(format!("=== MAKER (turn {}) ===", turn + 1), |s| s.cyan().bold()));
        maker_output = run_maker(&args.cwd, &feedback, true).await?;
        println!();

        if args.strip_ansi {
            maker_output = strip_ansi(&maker_output);
        }

        log_line("maker-out", &format!("{} bytes", maker_output.len()));

        turn += 1;

        if args.max_turns > 0 && turn >= args.max_turns {
            log_line("system", &format!("max_turns ({}) reached", args.max_turns));
            break;
        }
    }

    log_line("system", &format!("done after {} turn(s)", turn));

    Ok(())
}

fn critic_signaled_done(output: &str) -> bool {
    let trimmed = output.trim();
    trimmed == "ALL_DONE" || trimmed.to_uppercase() == "ALL_DONE"
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();

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
