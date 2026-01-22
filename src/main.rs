use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

mod tui;

#[derive(Parser, Debug)]
#[command(name = "leonard")]
#[command(about = "Relay text between Maker and Critic agents")]
struct Args {
    /// Working directory for both agents
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Overarching task to give the maker (omit for interactive TUI mode)
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

/// Run Claude in print mode and return its output
fn run_maker(cwd: &Option<PathBuf>, prompt: &str, is_continuation: bool) -> Result<String> {
    let mut cmd = Command::new("claude");
    cmd.arg("-p");
    cmd.arg("--dangerously-skip-permissions");
    cmd.arg("--permission-mode").arg("acceptEdits");

    if is_continuation {
        cmd.arg("--continue");
    }

    cmd.arg(prompt);

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    cmd.env("TERM", "xterm-256color");
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        cmd.env("ANTHROPIC_API_KEY", key);
    }

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    log_line("maker", &format!("prompt: {}...",
        if prompt.len() > 80 { &prompt[..80] } else { prompt }));

    let output = cmd.output().context("failed to run claude")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !stderr.is_empty() && !stderr.contains("Shell cwd was reset") {
        log_line("maker-err", &stderr);
    }

    if !output.status.success() {
        anyhow::bail!("maker exited with status: {}", output.status);
    }

    Ok(stdout)
}

/// Run Codex exec and return its output
fn run_critic(cwd: &Option<PathBuf>, prompt: &str) -> Result<String> {
    let mut cmd = Command::new("codex");
    cmd.arg("exec");
    cmd.arg("--full-auto");

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
        cmd.arg("-C").arg(dir);
    }

    cmd.arg(prompt);

    cmd.env("TERM", "xterm-256color");
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        cmd.env("OPENAI_API_KEY", key);
    }

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    log_line("critic", &format!("prompt: {}...",
        if prompt.len() > 80 { &prompt[..80] } else { prompt }));

    let output = cmd.output().context("failed to run codex")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !stderr.is_empty() {
        log_line("critic-err", &stderr);
    }

    if !output.status.success() {
        anyhow::bail!("critic exited with status: {}", output.status);
    }

    Ok(stdout)
}

pub fn truncate(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        text.to_string()
    } else {
        // Find a valid UTF-8 boundary at or after the target start position
        let target_start = text.len() - max_bytes;
        let start = text
            .char_indices()
            .map(|(i, _)| i)
            .find(|&i| i >= target_start)
            .unwrap_or(text.len());
        format!("[...truncated...]\n{}", &text[start..])
    }
}

fn run_batch(args: &Args, task: &str) -> Result<()> {
    log_line("system", &format!("task: {}", task));

    // First maker turn - just the task
    let mut maker_output = run_maker(&args.cwd, task, false)?;

    if args.strip_ansi {
        maker_output = strip_ansi(&maker_output);
    }

    log_line("maker-out", &format!("{} bytes", maker_output.len()));
    println!("=== MAKER ===\n{}\n", maker_output);

    let mut turn = 0;

    loop {

        // Forward maker output to critic verbatim
        let forward_text = truncate(&maker_output, args.max_forward_bytes);

        let mut critic_output = run_critic(&args.cwd, &forward_text)?;

        if args.strip_ansi {
            critic_output = strip_ansi(&critic_output);
        }

        log_line("critic-out", &format!("{} bytes", critic_output.len()));
        println!("=== CRITIC (turn {}) ===\n{}\n", turn, critic_output);

        // Forward critic output to maker verbatim
        let feedback = truncate(&critic_output, args.max_forward_bytes);

        maker_output = run_maker(&args.cwd, &feedback, true)?;

        if args.strip_ansi {
            maker_output = strip_ansi(&maker_output);
        }

        log_line("maker-out", &format!("{} bytes", maker_output.len()));
        println!("=== MAKER (turn {}) ===\n{}\n", turn + 1, maker_output);

        turn += 1;

        if args.max_turns > 0 && turn >= args.max_turns {
            log_line("system", &format!("max_turns ({}) reached", args.max_turns));
            break;
        }
    }

    log_line("system", &format!("done after {} turn(s)", turn));

    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();

    match &args.task {
        Some(task) => {
            // Batch mode with provided task
            run_batch(&args, task)
        }
        None => {
            // Interactive TUI mode
            tui::run_tui(
                args.cwd,
                None,
                args.max_turns,
                args.strip_ansi,
                args.max_forward_bytes,
            )
        }
    }
}
