use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
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
use std::io::{self, BufRead, BufReader, Stdout};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
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

fn truncate_line(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}...", truncated)
    }
}

#[derive(Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
    pub turn: usize,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(PartialEq, Clone, Copy)]
pub enum AppState {
    Running,
    Paused,
    Editing,
    WaitingForTask,
    Finished,
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
    pub streaming_content: String,
    pub streaming_tool_calls: Vec<ToolCall>,
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
            streaming_content: String::new(),
            streaming_tool_calls: Vec::new(),
        }
    }

    pub fn start_streaming(&mut self, role: &str) {
        self.streaming_role = Some(role.to_string());
        self.streaming_content.clear();
        self.streaming_tool_calls.clear();
    }

    pub fn append_streaming_line(&mut self, line: &str) {
        if !self.streaming_content.is_empty() {
            self.streaming_content.push('\n');
        }
        self.streaming_content.push_str(line);
    }

    pub fn add_streaming_tool_call(&mut self, tool_call: ToolCall) {
        self.streaming_tool_calls.push(tool_call);
    }

    pub fn update_streaming_tool_result(&mut self, tool_use_id: &str, summary: String) {
        if let Some(tc) = self.streaming_tool_calls.iter_mut().find(|tc| tc.id == tool_use_id) {
            tc.result_summary = Some(summary);
        }
    }

    pub fn finish_streaming(&mut self) -> Option<(String, String, Vec<ToolCall>)> {
        if let Some(role) = self.streaming_role.take() {
            let content = std::mem::take(&mut self.streaming_content);
            let tool_calls = std::mem::take(&mut self.streaming_tool_calls);
            Some((role, content, tool_calls))
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

    pub fn add_message(&mut self, role: &str, content: &str, tool_calls: Vec<ToolCall>) {
        self.messages.push(Message {
            role: role.to_string(),
            content: content.to_string(),
            turn: self.turn,
            tool_calls,
        });
    }
}

enum AgentResult {
    MakerLine(String),
    MakerToolCall(ToolCall),
    MakerToolResult { tool_use_id: String, summary: String },
    CriticLine(String),
    MakerDone,
    CriticDone,
    Error(String),
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
    cmd.stderr(Stdio::null());

    match cmd.spawn() {
        Ok(mut child) => {
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
                            ClaudeEvent::Result { result, .. } => {
                                // Final result text
                                if !result.is_empty() {
                                    let _ = tx.send(AgentResult::MakerLine(result));
                                }
                            }
                            ClaudeEvent::Unknown => {}
                        }
                    }
                    // Ignore unparseable lines (they might be partial JSON or other output)
                }
            }
            let status = child.wait();
            if let Ok(exit) = status {
                if !exit.success() {
                    let _ = tx.send(AgentResult::Error(format!("Maker exited with status: {}", exit)));
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

fn run_critic_streaming(
    cwd: Option<PathBuf>,
    prompt: String,
    tx: Sender<AgentResult>,
) {
    let mut cmd = Command::new("codex");
    cmd.arg("exec");
    cmd.arg("--full-auto");

    if let Some(dir) = &cwd {
        cmd.current_dir(dir);
        cmd.arg("-C").arg(dir);
    }

    cmd.arg(&prompt);

    cmd.env("TERM", "xterm-256color");
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        cmd.env("OPENAI_API_KEY", key);
    }

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());

    match cmd.spawn() {
        Ok(mut child) => {
            if let Some(stdout) = child.stdout.take() {
                let reader = BufReader::new(stdout);
                for line in reader.lines().flatten() {
                    let _ = tx.send(AgentResult::CriticLine(line));
                }
            }
            let status = child.wait();
            if let Ok(exit) = status {
                if !exit.success() {
                    let _ = tx.send(AgentResult::Error(format!("Critic exited with status: {}", exit)));
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
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(task.clone(), max_turns);

    let (tx, rx): (Sender<AgentResult>, Receiver<AgentResult>) = mpsc::channel();

    if let Some(ref task_prompt) = task {
        app.status_message = "Running maker...".to_string();
        app.request_in_flight = true;
        app.start_streaming("maker");
        let cwd_clone = cwd.clone();
        let task_clone = task_prompt.clone();
        let tx_clone = tx.clone();
        thread::spawn(move || {
            run_maker_streaming(cwd_clone, task_clone, false, tx_clone);
        });
    }

    let result = run_app(&mut terminal, &mut app, &tx, &rx, cwd.clone(), max_forward_bytes, strip_ansi_codes);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

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
                    app.append_streaming_line(&line);
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
                    app.append_streaming_line(&line);
                }
                AgentResult::MakerDone => {
                    app.request_in_flight = false;
                    if let Some((role, content, tool_calls)) = app.finish_streaming() {
                        app.add_message(&role, &content, tool_calls);

                        if app.state == AppState::Running {
                            app.status_message = "Running critic...".to_string();
                            app.request_in_flight = true;
                            app.start_streaming("critic");

                            let forward_text = truncate(&content, max_forward_bytes);
                            let cwd_clone = cwd.clone();
                            let tx_clone = tx.clone();
                            thread::spawn(move || {
                                run_critic_streaming(cwd_clone, forward_text, tx_clone);
                            });
                        } else {
                            app.status_message = "Paused. Press 'c' to continue, 'e' to edit, 'q' to quit.".to_string();
                        }
                    }
                }
                AgentResult::CriticDone => {
                    app.request_in_flight = false;
                    if let Some((role, content, tool_calls)) = app.finish_streaming() {
                        app.add_message(&role, &content, tool_calls);
                        app.turn += 1;

                        if app.max_turns > 0 && app.turn >= app.max_turns {
                            app.state = AppState::Finished;
                            app.status_message = format!("Finished after {} turns. Press 'q' to quit.", app.turn);
                        } else if app.state == AppState::Running {
                            app.status_message = "Running maker...".to_string();
                            app.request_in_flight = true;
                            app.start_streaming("maker");

                            let forward_text = truncate(&content, max_forward_bytes);
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
                    app.request_in_flight = false;
                    // Commit partial content if any
                    if let Some((role, content, tool_calls)) = app.finish_streaming() {
                        if !content.is_empty() {
                            app.add_message(&role, &format!("{}\n\n[Error: {}]", content, e), tool_calls);
                        }
                    }
                    app.status_message = format!("Error: {}", e);
                    app.state = AppState::Paused;
                }
            }
        }
        if new_content {
            app.scroll_to_bottom(visible_height);
        }

        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match app.state {
                    AppState::WaitingForTask => {
                        match key.code {
                            KeyCode::Char('q') => break,
                            KeyCode::Char(c) => {
                                app.edit_buffer.insert(app.edit_cursor, c);
                                app.edit_cursor += 1;
                            }
                            KeyCode::Backspace => {
                                if app.edit_cursor > 0 {
                                    app.edit_cursor -= 1;
                                    app.edit_buffer.remove(app.edit_cursor);
                                }
                            }
                            KeyCode::Left => {
                                if app.edit_cursor > 0 {
                                    app.edit_cursor -= 1;
                                }
                            }
                            KeyCode::Right => {
                                if app.edit_cursor < app.edit_buffer.len() {
                                    app.edit_cursor += 1;
                                }
                            }
                            KeyCode::Enter => {
                                if !app.edit_buffer.is_empty() {
                                    let task = app.edit_buffer.clone();
                                    app.task = Some(task.clone());
                                    app.edit_buffer.clear();
                                    app.edit_cursor = 0;
                                    app.state = AppState::Running;
                                    app.status_message = "Running maker...".to_string();
                                    app.request_in_flight = true;
                                    app.start_streaming("maker");

                                    let cwd_clone = cwd.clone();
                                    let tx_clone = tx.clone();
                                    thread::spawn(move || {
                                        run_maker_streaming(cwd_clone, task, false, tx_clone);
                                    });
                                }
                            }
                            KeyCode::Esc => break,
                            _ => {}
                        }
                    }
                    AppState::Editing => {
                        match key.code {
                            KeyCode::Esc => {
                                app.state = AppState::Paused;
                                app.edit_buffer.clear();
                                app.edit_cursor = 0;
                                app.editing_message_index = None;
                                app.status_message = "Edit cancelled. Press 'c' to continue.".to_string();
                            }
                            KeyCode::Char(c) => {
                                app.edit_buffer.insert(app.edit_cursor, c);
                                app.edit_cursor += 1;
                            }
                            KeyCode::Backspace => {
                                if app.edit_cursor > 0 {
                                    app.edit_cursor -= 1;
                                    app.edit_buffer.remove(app.edit_cursor);
                                }
                            }
                            KeyCode::Left => {
                                if app.edit_cursor > 0 {
                                    app.edit_cursor -= 1;
                                }
                            }
                            KeyCode::Right => {
                                if app.edit_cursor < app.edit_buffer.len() {
                                    app.edit_cursor += 1;
                                }
                            }
                            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                if !app.edit_buffer.is_empty() {
                                    let edited = app.edit_buffer.clone();

                                    // Update the displayed message with edited content
                                    if let Some(idx) = app.editing_message_index {
                                        if idx < app.messages.len() {
                                            app.messages[idx].content = edited.clone();
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
                                            app.start_streaming("critic");
                                            let forward_text = truncate(&edited, max_forward_bytes);
                                            let cwd_clone = cwd.clone();
                                            let tx_clone = tx.clone();
                                            thread::spawn(move || {
                                                run_critic_streaming(cwd_clone, forward_text, tx_clone);
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
                            KeyCode::Enter => {
                                app.edit_buffer.push('\n');
                                app.edit_cursor += 1;
                            }
                            _ => {}
                        }
                    }
                    AppState::Running | AppState::Paused | AppState::Finished => {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => break,
                            KeyCode::Char('p') if app.state == AppState::Running => {
                                app.state = AppState::Paused;
                                app.status_message = "Paused. Press 'c' to continue, 'e' to edit, 'q' to quit.".to_string();
                            }
                            KeyCode::Char('c') if app.state == AppState::Paused && !app.request_in_flight => {
                                app.state = AppState::Running;

                                if let Some(last) = app.messages.last() {
                                    let forward_text = truncate(&last.content, max_forward_bytes);
                                    let cwd_clone = cwd.clone();
                                    let tx_clone = tx.clone();
                                    app.request_in_flight = true;

                                    if last.role == "maker" {
                                        app.status_message = "Running critic...".to_string();
                                        app.start_streaming("critic");
                                        thread::spawn(move || {
                                            run_critic_streaming(cwd_clone, forward_text, tx_clone);
                                        });
                                    } else {
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
                                    app.edit_buffer = last.content.clone();
                                    app.edit_cursor = app.edit_buffer.len();
                                    app.editing_message_index = Some(app.messages.len() - 1);
                                    app.status_message = "Editing. Ctrl+Enter to send, Esc to cancel.".to_string();
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
            }
        }
    }

    Ok(())
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

            // Truncate task to fit in one line
            let max_task_len = chunks[0].width.saturating_sub(4) as usize;
            let display_task = if task.len() > max_task_len {
                format!("{}...", &task[..max_task_len.saturating_sub(3)])
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

            // Display tool calls if any
            if !msg.tool_calls.is_empty() {
                let tool_style = Style::default().fg(Color::Magenta);
                for tc in &msg.tool_calls {
                    let result_text = tc.result_summary.as_deref().unwrap_or("...");
                    lines.push(Line::from(Span::styled(
                        format!("  [{}] {}", tc.name, result_text),
                        tool_style,
                    )));
                }
            }

            for line in msg.content.lines() {
                lines.push(Line::from(Span::styled(truncate_line(line, 500), content_style)));
            }

            if i < app.messages.len() - 1 {
                lines.push(Line::from(""));
            }
        }

        // Show streaming content if any
        if let Some(ref role) = app.streaming_role {
            if !app.streaming_content.is_empty() || !app.streaming_tool_calls.is_empty() || app.request_in_flight {
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

                // Display streaming tool calls
                if !app.streaming_tool_calls.is_empty() {
                    let tool_style = Style::default().fg(Color::Magenta);
                    for tc in &app.streaming_tool_calls {
                        let result_text = tc.result_summary.as_deref().unwrap_or("...");
                        lines.push(Line::from(Span::styled(
                            format!("  [{}] {}", tc.name, result_text),
                            tool_style,
                        )));
                    }
                }

                for line in app.streaming_content.lines() {
                    lines.push(Line::from(Span::styled(truncate_line(line, 500), content_style)));
                }
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
        AppState::Running => "p:pause  q:quit  j/k:scroll",
        AppState::Paused if app.request_in_flight => "waiting...  q:quit  j/k:scroll",
        AppState::Paused => "c:continue  e:edit  q:quit  j/k:scroll",
        AppState::Editing => "Ctrl+Enter:send  Esc:cancel",
        AppState::WaitingForTask => "Enter:submit  Esc:quit",
        AppState::Finished => "q:quit  j/k:scroll",
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
