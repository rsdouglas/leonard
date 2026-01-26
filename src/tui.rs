use anyhow::Result;
use crossterm::{
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame, Terminal,
};
use serde::Deserialize;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Stdout, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::{strip_ansi, truncate};

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
    Result { result: String, #[allow(dead_code)] cost_usd: Option<f64> },
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
    ToolUse { id: String, name: String, #[allow(dead_code)] input: serde_json::Value },
    #[serde(rename = "tool_result")]
    ToolResult { tool_use_id: String, content: Option<serde_json::Value> },
    #[serde(other)]
    Unknown,
}

/// A tool call with its name and a summary of the result
#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub result_summary: Option<String>,
}

/// A single content item in a message - either text, a tool call, reasoning, or a command
#[derive(Clone, Debug)]
pub enum ContentItem {
    Text(String),
    ToolCall(ToolCall),
    Reasoning(String),
    Command(CriticCommand),
}

fn truncate_line(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}...", truncated)
    }
}

/// Format message items for forwarding to the other agent.
/// Produces the same format shown in the TUI.
fn format_message_output(items: &[ContentItem]) -> String {
    let mut output = String::new();

    for item in items {
        match item {
            ContentItem::Text(text) => {
                if !output.is_empty() && !output.ends_with('\n') {
                    output.push('\n');
                }
                output.push_str(text);
                if !text.ends_with('\n') {
                    output.push('\n');
                }
            }
            ContentItem::ToolCall(tc) => {
                let result_text = tc.result_summary.as_deref().unwrap_or("...");
                output.push_str(&format!("  [{}] {}\n", tc.name, truncate_line(result_text, 80)));
            }
            ContentItem::Reasoning(text) => {
                for line in text.lines() {
                    output.push_str(&format!("  thinking: {}\n", truncate_line(line, 80)));
                }
            }
            ContentItem::Command(cmd) => {
                let status_text = match &cmd.status {
                    CriticCommandStatus::InProgress => {
                        format!("  running: {}", truncate_line(&cmd.command, 60))
                    }
                    CriticCommandStatus::Completed { exit_code, output_summary } => {
                        if output_summary.is_empty() {
                            format!("  [exit {}] {}", exit_code, truncate_line(&cmd.command, 60))
                        } else {
                            format!("  [exit {}] {} -> {}", exit_code, truncate_line(&cmd.command, 40), truncate_line(output_summary, 30))
                        }
                    }
                };
                output.push_str(&status_text);
                output.push('\n');
            }
        }
    }

    output.trim_end().to_string()
}

/// Simple file logger for debugging prompts and responses
#[derive(Clone)]
pub struct Logger {
    file: Option<Arc<Mutex<File>>>,
}

impl Logger {
    pub fn new(path: Option<PathBuf>) -> Result<Self> {
        let file = match path {
            Some(p) => {
                let f = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)?;
                Some(Arc::new(Mutex::new(f)))
            }
            None => None,
        };
        Ok(Logger { file })
    }

    pub fn log(&self, label: &str, content: &str) {
        if let Some(ref file) = self.file {
            if let Ok(mut f) = file.lock() {
                let timestamp = chrono_lite_timestamp();
                let _ = writeln!(f, "\n=== {} [{}] ===\n{}\n", label, timestamp, content);
                let _ = f.flush();
            }
        }
    }
}

fn chrono_lite_timestamp() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}s", now.as_secs())
}

fn critic_signaled_done(items: &[ContentItem]) -> bool {
    // Check if any text item contains ALL_DONE
    for item in items {
        if let ContentItem::Text(text) = item {
            if text.lines().any(|line| line.trim() == "ALL_DONE") || text.contains("ALL_DONE") {
                return true;
            }
        }
    }
    false
}

/// Build the critic meta-prompt that frames the review context
fn build_critic_prompt(task: &str, maker_output: &str, is_continuation: bool) -> String {
    if is_continuation {
        // On continuation, critic already has context - just send new output
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
        // First call - full framing
        format!(
            r#"ROLE: Helpful Peer
You are acting as a helpful peer. Your job is to evaluate the maker's work for the task below.
Do not offer to do things. Discuss, comment, and guide the maker. 
Your job is not to block the maker, but to help them make progress and point out things they may have missed.
Progress is the goal, not perfection. We work iteratively, so we can improve incrementally.

## Original Task
{task}

## Maker's Output

---
{maker_output}
---

If the task is complete, you can end the conversation with "ALL_DONE".
"#,
            task = task,
            maker_output = maker_output
        )
    }
}

/// Convert a character index to a byte index in a string.
/// Returns s.len() if char_idx is at or beyond the end.
fn char_to_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

#[derive(Clone)]
pub struct Message {
    pub role: String,
    pub turn: usize,
    pub items: Vec<ContentItem>,
}

#[derive(PartialEq, Clone, Copy)]
pub enum AppState {
    Running,
    Paused,
    Editing,
    WaitingForTask,
    Finished,
}

/// A critic command with its status for display
#[derive(Clone, Debug)]
pub struct CriticCommand {
    pub command: String,
    pub status: CriticCommandStatus,
}

pub struct App {
    pub messages: Vec<Message>,
    pub state: AppState,
    pub scroll: u16,
    pub total_lines: u16,
    pub turn: usize,
    pub max_turns: usize,
    pub edit_buffer: String,
    pub edit_cursor: usize,
    pub status_message: String,
    pub task: Option<String>,
    pub request_in_flight: bool,
    pub editing_message_index: Option<usize>,
    pub streaming_role: Option<String>,
    pub streaming_items: Vec<ContentItem>,
    pub first_maker_call_made: bool,
    pub first_critic_call_made: bool,
}

impl App {
    pub fn new(task: Option<String>, max_turns: usize) -> Self {
        let state = if task.is_some() {
            AppState::Running
        } else {
            AppState::WaitingForTask
        };
        Self {
            messages: Vec::new(),
            state,
            scroll: 0,
            total_lines: 0,
            turn: 0,
            max_turns,
            edit_buffer: String::new(),
            edit_cursor: 0,
            status_message: String::new(),
            task,
            request_in_flight: false,
            editing_message_index: None,
            streaming_role: None,
            streaming_items: Vec::new(),
            first_maker_call_made: false,
            first_critic_call_made: false,
        }
    }

    pub fn start_streaming(&mut self, role: &str) {
        self.streaming_role = Some(role.to_string());
        self.streaming_items.clear();
    }

    pub fn append_streaming_text(&mut self, text: &str) {
        // Append to last text item if it exists, otherwise create new
        if let Some(ContentItem::Text(ref mut last_text)) = self.streaming_items.last_mut() {
            if !last_text.is_empty() {
                last_text.push('\n');
            }
            last_text.push_str(text);
        } else {
            self.streaming_items.push(ContentItem::Text(text.to_string()));
        }
    }

    pub fn add_streaming_tool_call(&mut self, tool_call: ToolCall) {
        self.streaming_items.push(ContentItem::ToolCall(tool_call));
    }

    pub fn update_streaming_tool_result(&mut self, tool_use_id: &str, summary: String) {
        for item in &mut self.streaming_items {
            if let ContentItem::ToolCall(ref mut tc) = item {
                if tc.id == tool_use_id {
                    tc.result_summary = Some(summary);
                    break;
                }
            }
        }
    }

    pub fn add_streaming_reasoning(&mut self, text: String) {
        self.streaming_items.push(ContentItem::Reasoning(text));
    }

    pub fn add_streaming_command(&mut self, command: String, status: CriticCommandStatus) {
        // Check if we already have this command (to update status)
        for item in &mut self.streaming_items {
            if let ContentItem::Command(ref mut cmd) = item {
                if cmd.command == command {
                    cmd.status = status;
                    return;
                }
            }
        }
        self.streaming_items.push(ContentItem::Command(CriticCommand { command, status }));
    }

    pub fn finish_streaming(&mut self) -> Option<(String, Vec<ContentItem>)> {
        if let Some(role) = self.streaming_role.take() {
            let items = std::mem::take(&mut self.streaming_items);
            Some((role, items))
        } else {
            None
        }
    }

    pub fn scroll_up(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_sub(amount);
    }

    pub fn scroll_down(&mut self, amount: u16, visible_height: u16) {
        let max_scroll = self.total_lines.saturating_sub(visible_height);
        self.scroll = (self.scroll + amount).min(max_scroll);
    }

    pub fn scroll_to_bottom(&mut self, visible_height: u16) {
        let max_scroll = self.total_lines.saturating_sub(visible_height);
        self.scroll = max_scroll;
    }

    pub fn add_message(&mut self, role: &str, items: Vec<ContentItem>) {
        self.messages.push(Message {
            role: role.to_string(),
            turn: self.turn,
            items,
        });
    }
}

enum AgentResult {
    MakerLine(String),
    MakerToolCall(ToolCall),
    MakerToolResult { tool_use_id: String, summary: String },
    CriticLine(String),
    CriticReasoning(String),
    CriticCommand { command: String, status: CriticCommandStatus },
    MakerDone,
    CriticDone,
    Error(String),
}

#[derive(Clone, Debug)]
pub enum CriticCommandStatus {
    InProgress,
    Completed { exit_code: i32, output_summary: String },
}

/// Codex JSONL event types (top-level)
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum CodexEvent {
    #[serde(rename = "item.completed")]
    ItemCompleted { item: CodexItem },
    #[serde(other)]
    Unknown,
}

/// Codex item types (nested inside item.completed)
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
        status: Option<String>,
        exit_code: Option<i32>,
        output: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

/// Summarize tool result content for display
fn summarize_tool_result(content: &Option<serde_json::Value>) -> String {
    match content {
        None => "done".to_string(),
        Some(serde_json::Value::String(s)) => {
            let lines: Vec<&str> = s.lines().collect();
            if lines.len() <= 3 {
                s.chars().take(100).collect::<String>()
                    + if s.len() > 100 { "..." } else { "" }
            } else {
                format!("{} lines", lines.len())
            }
        }
        Some(serde_json::Value::Array(arr)) => {
            // Check if it's an array of content blocks
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
                    combined.chars().take(100).collect::<String>()
                        + if combined.len() > 100 { "..." } else { "" }
                } else {
                    format!("{} lines", lines.len())
                }
            } else {
                format!("{} items", arr.len())
            }
        }
        Some(v) => {
            let s = v.to_string();
            s.chars().take(50).collect::<String>()
                + if s.len() > 50 { "..." } else { "" }
        }
    }
}

fn run_maker_streaming(
    cwd: Option<PathBuf>,
    prompt: String,
    is_continuation: bool,
    tx: Sender<AgentResult>,
) {
    if prompt.trim().is_empty() {
        let _ = tx.send(AgentResult::Error("Cannot run maker with empty prompt".to_string()));
        return;
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

    cmd.arg(&prompt);

    if let Some(dir) = &cwd {
        cmd.current_dir(dir);
    }

    cmd.env("TERM", "xterm-256color");
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        cmd.env("ANTHROPIC_API_KEY", key);
    }

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    match cmd.spawn() {
        Ok(mut child) => {
            // Read stderr in a separate thread to avoid blocking
            let stderr_handle = child.stderr.take().map(|stderr| {
                thread::spawn(move || {
                    let mut buf = String::new();
                    let mut reader = BufReader::new(stderr);
                    let _ = reader.read_to_string(&mut buf);
                    buf
                })
            });

            let mut error_lines = Vec::new();

            if let Some(stdout) = child.stdout.take() {
                let reader = BufReader::new(stdout);

                for line in reader.lines().flatten() {
                    // Try to parse as JSON event
                    if let Ok(event) = serde_json::from_str::<ClaudeEvent>(&line) {
                        match event {
                            ClaudeEvent::Assistant { message } => {
                                for block in message.content {
                                    match block {
                                        ContentBlock::Text { text } => {
                                            let _ = tx.send(AgentResult::MakerLine(text));
                                        }
                                        ContentBlock::ToolUse { id, name, .. } => {
                                            // Send tool call with pending result
                                            let _ = tx.send(AgentResult::MakerToolCall(ToolCall {
                                                id,
                                                name,
                                                result_summary: None,
                                            }));
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            ClaudeEvent::User { message } => {
                                for block in message.content {
                                    if let ContentBlock::ToolResult { tool_use_id, content } = block {
                                        // Send update for the specific tool by ID
                                        let summary = summarize_tool_result(&content);
                                        let _ = tx.send(AgentResult::MakerToolResult {
                                            tool_use_id,
                                            summary,
                                        });
                                    }
                                }
                            }
                            ClaudeEvent::Result { .. } => {
                                // Final result - ignore since we already captured via streaming Assistant events
                            }
                            ClaudeEvent::Unknown => {}
                        }
                    } else {
                        // Capture unparseable lines - might contain error messages
                        error_lines.push(line);
                    }
                }
            }
            let status = child.wait();

            // Collect stderr from thread
            let stderr_msg = stderr_handle
                .and_then(|h| h.join().ok())
                .unwrap_or_default();

            if let Ok(exit) = status {
                if !exit.success() {
                    let mut error_msg = format!("Maker (claude) exited with status: {}", exit);
                    if !stderr_msg.trim().is_empty() {
                        error_msg.push_str(&format!("\nstderr: {}", stderr_msg.trim()));
                    }
                    if !error_lines.is_empty() {
                        error_msg.push_str(&format!("\noutput: {}", error_lines.join("\n")));
                    }
                    let _ = tx.send(AgentResult::Error(error_msg));
                    return;
                }
            }
            let _ = tx.send(AgentResult::MakerDone);
        }
        Err(e) => {
            let _ = tx.send(AgentResult::Error(format!("Failed to spawn maker: {}", e)));
        }
    }
}

/// Summarize command output for display
fn summarize_command_output(output: &Option<String>) -> String {
    match output {
        None => String::new(),
        Some(s) => {
            let lines: Vec<&str> = s.lines().collect();
            if lines.len() <= 3 {
                s.chars().take(100).collect::<String>()
                    + if s.chars().count() > 100 { "..." } else { "" }
            } else {
                format!("{} lines", lines.len())
            }
        }
    }
}

fn run_critic_streaming(
    cwd: Option<PathBuf>,
    prompt: String,
    is_continuation: bool,
    tx: Sender<AgentResult>,
) {
    if prompt.trim().is_empty() {
        let _ = tx.send(AgentResult::Error("Cannot run critic with empty prompt".to_string()));
        return;
    }

    let mut cmd = Command::new("codex");
    cmd.arg("exec");

    if is_continuation {
        cmd.arg("resume");
        cmd.arg("--last");
        cmd.arg("--json");
        cmd.arg(&prompt);
    } else {
        cmd.arg("--sandbox").arg("read-only");
        cmd.arg("--json");
        if let Some(dir) = &cwd {
            cmd.arg("-C").arg(dir);
        }
        cmd.arg(&prompt);
    }

    if let Some(dir) = &cwd {
        cmd.current_dir(dir);
    }

    cmd.env("TERM", "xterm-256color");
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        cmd.env("OPENAI_API_KEY", key);
    }

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    match cmd.spawn() {
        Ok(mut child) => {
            // Read stderr in a separate thread to avoid blocking
            let stderr_handle = child.stderr.take().map(|stderr| {
                thread::spawn(move || {
                    let mut buf = String::new();
                    let mut reader = BufReader::new(stderr);
                    let _ = reader.read_to_string(&mut buf);
                    buf
                })
            });

            let mut error_lines = Vec::new();

            if let Some(stdout) = child.stdout.take() {
                let reader = BufReader::new(stdout);
                for line in reader.lines().flatten() {
                    // Try to parse as JSON event
                    if let Ok(event) = serde_json::from_str::<CodexEvent>(&line) {
                        match event {
                            CodexEvent::ItemCompleted { item } => {
                                match item {
                                    CodexItem::Reasoning { text } => {
                                        if let Some(t) = text {
                                            if !t.is_empty() {
                                                let _ = tx.send(AgentResult::CriticReasoning(t));
                                            }
                                        }
                                    }
                                    CodexItem::AgentMessage { text } => {
                                        if let Some(t) = text {
                                            if !t.is_empty() {
                                                let _ = tx.send(AgentResult::CriticLine(t));
                                            }
                                        }
                                    }
                                    CodexItem::CommandExecution { command, status, exit_code, output } => {
                                        let cmd_str = command.unwrap_or_default();
                                        if !cmd_str.is_empty() {
                                            let status_str = status.as_deref().unwrap_or("unknown");
                                            if status_str == "in_progress" {
                                                let _ = tx.send(AgentResult::CriticCommand {
                                                    command: cmd_str,
                                                    status: CriticCommandStatus::InProgress,
                                                });
                                            } else if status_str == "completed" {
                                                let output_summary = summarize_command_output(&output);
                                                let _ = tx.send(AgentResult::CriticCommand {
                                                    command: cmd_str,
                                                    status: CriticCommandStatus::Completed {
                                                        exit_code: exit_code.unwrap_or(0),
                                                        output_summary,
                                                    },
                                                });
                                            }
                                        }
                                    }
                                    CodexItem::Unknown => {}
                                }
                            }
                            CodexEvent::Unknown => {}
                        }
                    } else {
                        // Capture unparseable lines - might contain error messages
                        error_lines.push(line);
                    }
                }
            }
            let status = child.wait();

            // Collect stderr from thread
            let stderr_msg = stderr_handle
                .and_then(|h| h.join().ok())
                .unwrap_or_default();

            if let Ok(exit) = status {
                if !exit.success() {
                    let mut error_msg = format!("Critic (codex) exited with status: {}", exit);
                    if !stderr_msg.trim().is_empty() {
                        error_msg.push_str(&format!("\nstderr: {}", stderr_msg.trim()));
                    }
                    if !error_lines.is_empty() {
                        error_msg.push_str(&format!("\noutput: {}", error_lines.join("\n")));
                    }
                    let _ = tx.send(AgentResult::Error(error_msg));
                    return;
                }
            }
            let _ = tx.send(AgentResult::CriticDone);
        }
        Err(e) => {
            let _ = tx.send(AgentResult::Error(format!("Failed to spawn critic: {}", e)));
        }
    }
}

pub fn run_tui(
    cwd: Option<PathBuf>,
    task: Option<String>,
    max_turns: usize,
    strip_ansi_codes: bool,
    max_forward_bytes: usize,
    resume_session: bool,
    log_file: Option<PathBuf>,
) -> Result<()> {
    let logger = Logger::new(log_file)?;
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(task.clone(), max_turns);

    let (tx, rx): (Sender<AgentResult>, Receiver<AgentResult>) = mpsc::channel();

    if let Some(ref task_prompt) = task {
        app.status_message = "Running maker...".to_string();
        app.request_in_flight = true;
        app.first_maker_call_made = true;
        app.start_streaming("maker");
        logger.log("MAKER_PROMPT (initial)", task_prompt);
        let cwd_clone = cwd.clone();
        let task_clone = task_prompt.clone();
        let tx_clone = tx.clone();
        thread::spawn(move || {
            run_maker_streaming(cwd_clone, task_clone, resume_session, tx_clone);
        });
    }

    let result = run_app(&mut terminal, &mut app, &tx, &rx, cwd.clone(), max_forward_bytes, strip_ansi_codes, resume_session, &logger);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableBracketedPaste)?;

    result
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    tx: &Sender<AgentResult>,
    rx: &Receiver<AgentResult>,
    cwd: Option<PathBuf>,
    max_forward_bytes: usize,
    strip_ansi_codes: bool,
    resume_session: bool,
    logger: &Logger,
) -> Result<()> {
    let mut visible_height: u16 = 10;

    loop {
        terminal.draw(|f| {
            visible_height = ui(f, app);
        })?;

        let mut new_content = false;
        while let Ok(result) = rx.try_recv() {
            match result {
                AgentResult::MakerLine(mut line) => {
                    if strip_ansi_codes {
                        line = strip_ansi(&line);
                    }
                    new_content = true;
                    app.append_streaming_text(&line);
                }
                AgentResult::MakerToolCall(tool_call) => {
                    new_content = true;
                    app.add_streaming_tool_call(tool_call);
                }
                AgentResult::MakerToolResult { tool_use_id, summary } => {
                    new_content = true;
                    app.update_streaming_tool_result(&tool_use_id, summary);
                }
                AgentResult::CriticLine(mut line) => {
                    if strip_ansi_codes {
                        line = strip_ansi(&line);
                    }
                    new_content = true;
                    app.append_streaming_text(&line);
                }
                AgentResult::CriticReasoning(mut text) => {
                    if strip_ansi_codes {
                        text = strip_ansi(&text);
                    }
                    new_content = true;
                    app.add_streaming_reasoning(text);
                }
                AgentResult::CriticCommand { command, status } => {
                    new_content = true;
                    app.add_streaming_command(command, status);
                }
                AgentResult::MakerDone => {
                    app.request_in_flight = false;
                    if let Some((role, items)) = app.finish_streaming() {
                        app.add_message(&role, items.clone());

                        if app.state == AppState::Running {
                            app.status_message = "Running critic...".to_string();
                            app.request_in_flight = true;
                            // Use resume_session only for the very first critic call
                            let is_continuation = if app.first_critic_call_made {
                                true
                            } else {
                                app.first_critic_call_made = true;
                                resume_session
                            };
                            app.start_streaming("critic");

                            // Format exactly as TUI displays and wrap in reviewer prompt
                            let formatted = format_message_output(&items);
                            let task = app.task.as_deref().unwrap_or("");
                            let critic_prompt = build_critic_prompt(task, &formatted, is_continuation);
                            let forward_text = truncate(&critic_prompt, max_forward_bytes);
                            logger.log(&format!("CRITIC_PROMPT (cont={})", is_continuation), &forward_text);
                            let cwd_clone = cwd.clone();
                            let tx_clone = tx.clone();
                            thread::spawn(move || {
                                run_critic_streaming(cwd_clone, forward_text, is_continuation, tx_clone);
                            });
                        } else {
                            app.status_message = "Paused. Press 'c' to continue, 'e' to edit, 'q' to quit.".to_string();
                        }
                    }
                }
                AgentResult::CriticDone => {
                    app.request_in_flight = false;
                    if let Some((role, items)) = app.finish_streaming() {
                        app.add_message(&role, items.clone());
                        app.turn += 1;

                        // Check if critic signaled completion
                        if critic_signaled_done(&items) {
                            app.state = AppState::Finished;
                            app.status_message = format!("Critic signaled ALL_DONE. Press 'q' to quit.");
                        } else if app.max_turns > 0 && app.turn >= app.max_turns {
                            app.state = AppState::Finished;
                            app.status_message = format!("Finished after {} turns. Press 'q' to quit.", app.turn);
                        } else if app.state == AppState::Running {
                            app.status_message = "Running maker...".to_string();
                            app.request_in_flight = true;
                            app.start_streaming("maker");

                            // Format critic output exactly as TUI displays
                            let formatted = format_message_output(&items);
                            let forward_text = truncate(&formatted, max_forward_bytes);
                            logger.log("MAKER_PROMPT (after critic)", &forward_text);
                            let cwd_clone = cwd.clone();
                            let tx_clone = tx.clone();
                            thread::spawn(move || {
                                run_maker_streaming(cwd_clone, forward_text, true, tx_clone);
                            });
                        } else {
                            app.status_message = "Paused. Press 'c' to continue, 'q' to quit.".to_string();
                        }
                    }
                }
                AgentResult::Error(e) => {
                    // Exit TUI and report error to stderr
                    anyhow::bail!("{}", e);
                }
            }
        }
        if new_content {
            app.scroll_to_bottom(visible_height);
        }

        if event::poll(std::time::Duration::from_millis(100))? {
            match event::read()? {
                Event::Paste(text) => {
                    // Handle pasted text - insert at cursor position
                    if app.state == AppState::WaitingForTask || app.state == AppState::Editing {
                        let byte_idx = char_to_byte_index(&app.edit_buffer, app.edit_cursor);
                        app.edit_buffer.insert_str(byte_idx, &text);
                        app.edit_cursor += text.chars().count();
                    }
                }
                Event::Key(key) => match app.state {
                    AppState::WaitingForTask => {
                        match key.code {
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                            KeyCode::Char(c) => {
                                let byte_idx = char_to_byte_index(&app.edit_buffer, app.edit_cursor);
                                app.edit_buffer.insert(byte_idx, c);
                                app.edit_cursor += 1;
                            }
                            KeyCode::Backspace => {
                                if app.edit_cursor > 0 {
                                    app.edit_cursor -= 1;
                                    let byte_idx = char_to_byte_index(&app.edit_buffer, app.edit_cursor);
                                    app.edit_buffer.remove(byte_idx);
                                }
                            }
                            KeyCode::Left => {
                                if app.edit_cursor > 0 {
                                    app.edit_cursor -= 1;
                                }
                            }
                            KeyCode::Right => {
                                if app.edit_cursor < app.edit_buffer.chars().count() {
                                    app.edit_cursor += 1;
                                }
                            }
                            KeyCode::Enter => {
                                // Submit task
                                if !app.edit_buffer.is_empty() {
                                    let task = app.edit_buffer.clone();
                                    app.task = Some(task.clone());
                                    app.edit_buffer.clear();
                                    app.edit_cursor = 0;
                                    app.state = AppState::Running;
                                    app.status_message = "Running maker...".to_string();
                                    app.request_in_flight = true;
                                    // Use resume_session only for the very first maker call
                                    let is_continuation = if app.first_maker_call_made {
                                        true
                                    } else {
                                        app.first_maker_call_made = true;
                                        resume_session
                                    };
                                    app.start_streaming("maker");

                                    let cwd_clone = cwd.clone();
                                    let tx_clone = tx.clone();
                                    thread::spawn(move || {
                                        run_maker_streaming(cwd_clone, task, is_continuation, tx_clone);
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                    AppState::Editing => {
                        match key.code {
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                app.state = AppState::Paused;
                                app.edit_buffer.clear();
                                app.edit_cursor = 0;
                                app.editing_message_index = None;
                                app.status_message = "Edit cancelled. Press 'c' to continue.".to_string();
                            }
                            KeyCode::Char(c) => {
                                let byte_idx = char_to_byte_index(&app.edit_buffer, app.edit_cursor);
                                app.edit_buffer.insert(byte_idx, c);
                                app.edit_cursor += 1;
                            }
                            KeyCode::Backspace => {
                                if app.edit_cursor > 0 {
                                    app.edit_cursor -= 1;
                                    let byte_idx = char_to_byte_index(&app.edit_buffer, app.edit_cursor);
                                    app.edit_buffer.remove(byte_idx);
                                }
                            }
                            KeyCode::Left => {
                                if app.edit_cursor > 0 {
                                    app.edit_cursor -= 1;
                                }
                            }
                            KeyCode::Right => {
                                if app.edit_cursor < app.edit_buffer.chars().count() {
                                    app.edit_cursor += 1;
                                }
                            }
                            KeyCode::Enter => {
                                // Submit edit
                                if !app.edit_buffer.is_empty() {
                                    let edited = app.edit_buffer.clone();

                                    // Update the displayed message with edited content as single text item
                                    if let Some(idx) = app.editing_message_index {
                                        if idx < app.messages.len() {
                                            app.messages[idx].items = vec![ContentItem::Text(edited.clone())];
                                        }
                                    }

                                    app.edit_buffer.clear();
                                    app.edit_cursor = 0;
                                    app.editing_message_index = None;
                                    app.state = AppState::Running;
                                    app.request_in_flight = true;

                                    let last_role = app.messages.last().map(|m| m.role.as_str());

                                    match last_role {
                                        Some("maker") | None => {
                                            app.status_message = "Running critic with edited message...".to_string();
                                            let is_continuation = if app.first_critic_call_made {
                                                true
                                            } else {
                                                app.first_critic_call_made = true;
                                                resume_session
                                            };
                                            app.start_streaming("critic");
                                            let task = app.task.as_deref().unwrap_or("");
                                            let critic_prompt = build_critic_prompt(task, &edited, is_continuation);
                                            let forward_text = truncate(&critic_prompt, max_forward_bytes);
                                            let cwd_clone = cwd.clone();
                                            let tx_clone = tx.clone();
                                            thread::spawn(move || {
                                                run_critic_streaming(cwd_clone, forward_text, is_continuation, tx_clone);
                                            });
                                        }
                                        Some("critic") => {
                                            app.status_message = "Running maker with edited message...".to_string();
                                            app.start_streaming("maker");
                                            let forward_text = truncate(&edited, max_forward_bytes);
                                            let cwd_clone = cwd.clone();
                                            let tx_clone = tx.clone();
                                            thread::spawn(move || {
                                                run_maker_streaming(cwd_clone, forward_text, true, tx_clone);
                                            });
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    AppState::Running | AppState::Paused | AppState::Finished => {
                        match key.code {
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                            KeyCode::Char('p') if app.state == AppState::Running => {
                                app.state = AppState::Paused;
                                app.status_message = "Paused. Press 'c' to continue, 'e' to edit, ^C to quit.".to_string();
                            }
                            KeyCode::Char('c') if app.state == AppState::Paused && !app.request_in_flight => {
                                app.state = AppState::Running;

                                if let Some(last) = app.messages.last() {
                                    let cwd_clone = cwd.clone();
                                    let tx_clone = tx.clone();
                                    app.request_in_flight = true;

                                    if last.role == "maker" {
                                        // Format exactly as TUI displays and wrap in reviewer prompt
                                        let formatted = format_message_output(&last.items);
                                        let task = app.task.as_deref().unwrap_or("");
                                        app.status_message = "Running critic...".to_string();
                                        let is_continuation = if app.first_critic_call_made {
                                            true
                                        } else {
                                            app.first_critic_call_made = true;
                                            resume_session
                                        };
                                        let critic_prompt = build_critic_prompt(task, &formatted, is_continuation);
                                        let forward_text = truncate(&critic_prompt, max_forward_bytes);
                                        app.start_streaming("critic");
                                        thread::spawn(move || {
                                            run_critic_streaming(cwd_clone, forward_text, is_continuation, tx_clone);
                                        });
                                    } else {
                                        // Format critic output exactly as TUI displays for maker
                                        let formatted = format_message_output(&last.items);
                                        let forward_text = truncate(&formatted, max_forward_bytes);
                                        app.status_message = "Running maker...".to_string();
                                        app.start_streaming("maker");
                                        thread::spawn(move || {
                                            run_maker_streaming(cwd_clone, forward_text, true, tx_clone);
                                        });
                                    }
                                } else {
                                    app.status_message = "No messages to continue from.".to_string();
                                    app.state = AppState::Paused;
                                }
                            }
                            KeyCode::Char('e') if app.state == AppState::Paused && !app.request_in_flight => {
                                if let Some(last) = app.messages.last() {
                                    app.state = AppState::Editing;
                                    // Extract text content from items for editing
                                    app.edit_buffer = format_message_output(&last.items);
                                    app.edit_cursor = app.edit_buffer.chars().count();
                                    app.editing_message_index = Some(app.messages.len() - 1);
                                    app.status_message = "Editing. ^Enter to send, ^C to cancel.".to_string();
                                }
                            }
                            KeyCode::Up | KeyCode::Char('k') => app.scroll_up(1),
                            KeyCode::Down | KeyCode::Char('j') => app.scroll_down(1, visible_height),
                            KeyCode::PageUp => app.scroll_up(10),
                            KeyCode::PageDown => app.scroll_down(10, visible_height),
                            KeyCode::Home => app.scroll = 0,
                            KeyCode::End => app.scroll_to_bottom(visible_height),
                            _ => {}
                        }
                    }
                }
                _ => {} // Ignore other events (resize, focus, mouse, etc.)
            }
        }
    }

    Ok(())
}

/// Render content items to lines for display
fn render_items_to_lines(items: &[ContentItem], content_style: Style, lines: &mut Vec<Line<'_>>) {
    let tool_style = Style::default().fg(Color::Green);
    let reasoning_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM);
    let cmd_style = Style::default().fg(Color::Green);

    for item in items {
        match item {
            ContentItem::Text(text) => {
                for line in text.lines() {
                    lines.push(Line::from(Span::styled(truncate_line(line, 500), content_style)));
                }
            }
            ContentItem::ToolCall(tc) => {
                let result_text = tc.result_summary.as_deref().unwrap_or("...");
                lines.push(Line::from(Span::styled(
                    format!("  [{}] {}", tc.name, truncate_line(result_text, 80)),
                    tool_style,
                )));
            }
            ContentItem::Reasoning(text) => {
                for line in text.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("  thinking: {}", truncate_line(line, 80)),
                        reasoning_style,
                    )));
                }
            }
            ContentItem::Command(cmd) => {
                let status_text = match &cmd.status {
                    CriticCommandStatus::InProgress => {
                        format!("  running: {}", truncate_line(&cmd.command, 60))
                    }
                    CriticCommandStatus::Completed { exit_code, output_summary } => {
                        if output_summary.is_empty() {
                            format!("  [exit {}] {}", exit_code, truncate_line(&cmd.command, 60))
                        } else {
                            format!("  [exit {}] {} -> {}", exit_code, truncate_line(&cmd.command, 40), truncate_line(output_summary, 30))
                        }
                    }
                };
                lines.push(Line::from(Span::styled(status_text, cmd_style)));
            }
        }
    }
}

fn ui(f: &mut Frame, app: &mut App) -> u16 {
    // Calculate layout based on whether we have a task to display
    let has_task = app.task.is_some() && app.state != AppState::WaitingForTask;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if has_task {
            vec![
                Constraint::Length(3),  // Task display
                Constraint::Min(3),     // Messages
                Constraint::Length(3),  // Status bar
            ]
        } else {
            vec![
                Constraint::Length(0),  // No task display
                Constraint::Min(3),     // Messages/input
                Constraint::Length(3),  // Status bar
            ]
        })
        .split(f.size());

    // Render task display if we have one
    if has_task {
        if let Some(ref task) = app.task {
            let task_block = Block::default()
                .borders(Borders::ALL)
                .title(" Task ");

            // Truncate task to fit in one line (UTF-8 safe)
            let max_task_len = chunks[0].width.saturating_sub(4) as usize;
            let display_task = if task.chars().count() > max_task_len {
                let truncated: String = task.chars().take(max_task_len.saturating_sub(3)).collect();
                format!("{}...", truncated)
            } else {
                task.clone()
            };

            let task_para = Paragraph::new(display_task)
                .block(task_block)
                .style(Style::default().fg(Color::White));

            f.render_widget(task_para, chunks[0]);
        }
    }

    let content_height = chunks[1].height.saturating_sub(2); // Account for borders

    if app.state == AppState::WaitingForTask {
        let input_block = Block::default()
            .borders(Borders::ALL)
            .title(" Enter Task ");

        let input_text = Paragraph::new(app.edit_buffer.as_str())
            .block(input_block)
            .wrap(Wrap { trim: false });
        f.render_widget(input_text, chunks[1]);

        // Guard against divide-by-zero for very narrow terminals
        let usable_width = chunks[1].width.saturating_sub(2).max(1);
        let cursor_x = chunks[1].x + 1 + (app.edit_cursor as u16 % usable_width);
        let cursor_y = chunks[1].y + 1 + (app.edit_cursor as u16 / usable_width);
        f.set_cursor(cursor_x, cursor_y);
    } else {
        let messages_block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" Leonard - Turn {} ", app.turn));

        // Build content as lines for Paragraph
        let mut lines: Vec<Line> = Vec::new();
        for (i, msg) in app.messages.iter().enumerate() {
            let (header_style, content_style) = if msg.role == "maker" {
                (
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    Style::default().fg(Color::Cyan),
                )
            } else {
                (
                    Style::default().fg(Color::Rgb(255, 165, 0)).add_modifier(Modifier::BOLD),
                    Style::default().fg(Color::Yellow),
                )
            };

            lines.push(Line::from(Span::styled(
                format!("=== {} (turn {}) ===", msg.role.to_uppercase(), msg.turn),
                header_style,
            )));

            // Render items in order
            render_items_to_lines(&msg.items, content_style, &mut lines);

            if i < app.messages.len() - 1 {
                lines.push(Line::from(""));
            }
        }

        // Show streaming content if any
        if let Some(ref role) = app.streaming_role {
            let has_streaming_content = !app.streaming_items.is_empty() || app.request_in_flight;
            if has_streaming_content {
                let (header_style, content_style) = if role == "maker" {
                    (
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        Style::default().fg(Color::Cyan),
                    )
                } else {
                    (
                        Style::default().fg(Color::Rgb(255, 165, 0)).add_modifier(Modifier::BOLD),
                        Style::default().fg(Color::Yellow),
                    )
                };

                if !app.messages.is_empty() {
                    lines.push(Line::from(""));
                }

                lines.push(Line::from(Span::styled(
                    format!("=== {} (turn {}) [streaming...] ===", role.to_uppercase(), app.turn),
                    header_style,
                )));

                // Render streaming items in order
                render_items_to_lines(&app.streaming_items, content_style, &mut lines);
            }
        }

        app.total_lines = lines.len() as u16;

        let paragraph = Paragraph::new(lines)
            .block(messages_block)
            .wrap(Wrap { trim: false })
            .scroll((app.scroll, 0));

        f.render_widget(paragraph, chunks[1]);
    }

    // Status bar
    let state_str = match app.state {
        AppState::Running => "RUNNING",
        AppState::Paused => "PAUSED",
        AppState::Editing => "EDITING",
        AppState::WaitingForTask => "ENTER TASK",
        AppState::Finished => "FINISHED",
    };

    let help_text = match app.state {
        AppState::Running => "p:pause  ^C:quit  j/k:scroll",
        AppState::Paused if app.request_in_flight => "waiting...  ^C:quit  j/k:scroll",
        AppState::Paused => "c:continue  e:edit  ^C:quit  j/k:scroll",
        AppState::Editing => "Enter:send  ^C:cancel",
        AppState::WaitingForTask => "Enter:submit  ^C:quit",
        AppState::Finished => "^C:quit  j/k:scroll",
    };

    let status = Paragraph::new(Line::from(vec![
        Span::styled(
            format!(" [{}] ", state_str),
            Style::default().fg(Color::Black).bg(match app.state {
                AppState::Running => Color::Green,
                AppState::Paused => Color::Yellow,
                AppState::Editing => Color::Blue,
                AppState::WaitingForTask => Color::Magenta,
                AppState::Finished => Color::Gray,
            }),
        ),
        Span::raw(" "),
        Span::raw(&app.status_message),
        Span::raw(" | "),
        Span::styled(help_text, Style::default().fg(Color::DarkGray)),
    ]))
    .block(Block::default().borders(Borders::ALL));

    f.render_widget(status, chunks[2]);

    // Edit overlay
    if app.state == AppState::Editing {
        let area = centered_rect(80, 60, f.size());
        f.render_widget(Clear, area);

        let edit_block = Block::default()
            .borders(Borders::ALL)
            .title(" Edit Message (Ctrl+Enter to send) ");

        let edit_text = Paragraph::new(app.edit_buffer.as_str())
            .block(edit_block)
            .wrap(Wrap { trim: false });

        f.render_widget(edit_text, area);
    }

    content_height
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
