use anyhow::{Context, Result};
use clap::Parser;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

#[derive(Parser, Debug)]
#[command(name = "leonard")]
#[command(about = "Relay text between Maker and Critic agents")]
struct Args {
    /// Working directory for both agents
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Overarching task to give the maker
    #[arg(long)]
    task: String,

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

/// Run Claude in print mode and return its output
fn run_maker(
    cwd: &Option<PathBuf>,
    prompt: &str,
    is_continuation: bool,
    stream_strip_ansi: bool,
) -> Result<String> {
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

    let prompt_preview: String = prompt.chars().take(80).collect();
    log_line("maker", &format!("prompt: {}{}",
        prompt_preview,
        if prompt.chars().count() > 80 { "..." } else { "" }));

    let mut child = cmd.spawn().context("failed to spawn claude")?;
    let stdout = child.stdout.take().context("missing maker stdout")?;
    let stderr = child.stderr.take().context("missing maker stderr")?;

    let stderr_handle = thread::spawn(move || read_to_string(stderr));
    let stdout = stream_and_collect(stdout, stream_strip_ansi)?;
    let status = child.wait().context("failed to wait for claude")?;
    let stderr = join_reader(stderr_handle);

    if !stderr.is_empty() && !stderr.contains("Shell cwd was reset") {
        log_line("maker-err", &stderr);
    }

    if !status.success() {
        anyhow::bail!("maker exited with status: {}", status);
    }

    Ok(stdout)
}

/// Build the critic meta-prompt that frames the review context
fn build_critic_prompt(_task: &str, maker_output: &str, _is_continuation: bool) -> String {
    return maker_output.to_string();
}
//     if is_continuation {
//         // On continuation, critic already has context - just send new output
//         format!(
//             r#"The maker has responded:

// ---
// {maker_output}
// ---
// "#,
//             maker_output = maker_output
//         )
//     } else {
//         // First call - full framing
//         format!(
//             r#"ROLE: Helpful Peer
// You are acting as a helpful peer. Your job is to evaluate the maker's work for the task below.
// Do not offer to do things. Discuss, comment, and guide the maker. 
// Be friendly and helpful, not an overly harsh critic.
// Your job is not to block the maker, but to help them make progress and point out things they may have missed.
// Progress is the goal, not perfection. We work iteratively, so we can improve incrementally.

// ## Original Task
// {task}

// ## Maker's Output

// ---
// {maker_output}
// ---

// If the task is complete, you can end the conversation with "ALL_DONE".
// "#,
//             task = task,
//             maker_output = maker_output
//         )
//     }
// }

/// Run Codex exec and return its output (read-only sandbox)
fn run_critic(cwd: &Option<PathBuf>, prompt: &str, is_continuation: bool, stream_strip_ansi: bool) -> Result<String> {
    let mut cmd = Command::new("codex");
    cmd.arg("exec");

    if is_continuation {
        cmd.arg("resume");
        cmd.arg("--last");
        cmd.arg(prompt);
    } else {
        cmd.arg("--sandbox").arg("read-only");
        if let Some(dir) = cwd {
            cmd.arg("-C").arg(dir);
        }
        cmd.arg(prompt);
    }

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    cmd.env("TERM", "xterm-256color");
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        cmd.env("OPENAI_API_KEY", key);
    }

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let prompt_preview: String = prompt.chars().take(80).collect();
    log_line("critic", &format!("prompt: {}{}",
        prompt_preview,
        if prompt.chars().count() > 80 { "..." } else { "" }));

    let mut child = cmd.spawn().context("failed to spawn codex")?;
    let stdout = child.stdout.take().context("missing critic stdout")?;
    let stderr = child.stderr.take().context("missing critic stderr")?;

    let stderr_handle = thread::spawn(move || read_to_string(stderr));
    let stdout = stream_and_collect(stdout, stream_strip_ansi)?;
    let status = child.wait().context("failed to wait for codex")?;
    let stderr = join_reader(stderr_handle);

    if !stderr.is_empty() {
        log_line("critic-err", &stderr);
    }

    if !status.success() {
        anyhow::bail!("critic exited with status: {}", status);
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

    // First maker turn - just the task (use --continue if resuming previous session)
    println!("=== MAKER ===");
    let mut maker_output = run_maker(&args.cwd, task, args.r#continue, args.strip_ansi)?;
    println!();

    if args.strip_ansi {
        maker_output = strip_ansi(&maker_output);
    }

    log_line("maker-out", &format!("{} bytes", maker_output.len()));

    let mut turn = 0;

    loop {

        // First critic call (turn 0) uses resume only if --continue was passed
        let critic_is_continuation = turn > 0 || args.r#continue;

        // Build critic prompt with meta-context
        let truncated_maker = truncate(&maker_output, args.max_forward_bytes);
        let critic_prompt = build_critic_prompt(task, &truncated_maker, critic_is_continuation);

        println!("=== CRITIC (turn {}) ===", turn);
        let mut critic_output = run_critic(&args.cwd, &critic_prompt, critic_is_continuation, args.strip_ansi)?;
        println!();

        if args.strip_ansi {
            critic_output = strip_ansi(&critic_output);
        }

        log_line("critic-out", &format!("{} bytes", critic_output.len()));

        // Debug: show the actual lines for debugging
        log_line("critic-debug", &format!("Checking for ALL_DONE in {} lines", critic_output.lines().count()));
        for (idx, line) in critic_output.lines().enumerate().take(20) {
            log_line("critic-debug", &format!("Line {}: {:?}", idx, line));
        }

        if critic_signaled_done(&critic_output) {
            log_line("system", "critic signaled ALL_DONE; ending loop");
            break;
        }

        // Forward critic output to maker verbatim
        let feedback = truncate(&critic_output, args.max_forward_bytes);

        println!("=== MAKER (turn {}) ===", turn + 1);
        maker_output = run_maker(&args.cwd, &feedback, true, args.strip_ansi)?;
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
    // Check each line for ALL_DONE (case-insensitive, allowing for extra whitespace)
    output.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == "ALL_DONE" || trimmed.to_uppercase() == "ALL_DONE"
    }) || output.contains("ALL_DONE")  // Fallback: check if it appears anywhere
}

fn stream_and_collect<R: Read>(reader: R, strip_for_print: bool) -> Result<String> {
    let mut reader = BufReader::new(reader);
    let mut stdout = io::stdout();
    let mut collected = Vec::new();
    let mut buf = [0u8; 256];

    loop {
        let bytes_read = reader.read(&mut buf)?;
        if bytes_read == 0 {
            break;
        }
        let chunk = &buf[..bytes_read];
        collected.extend_from_slice(chunk);

        // Print as we receive data
        if strip_for_print {
            let text = String::from_utf8_lossy(chunk);
            let stripped = strip_ansi(&text);
            print!("{}", stripped);
        } else {
            // Write raw bytes directly to stdout
            let _ = stdout.write_all(chunk);
        }
        let _ = stdout.flush();
    }

    Ok(String::from_utf8_lossy(&collected).to_string())
}

fn read_to_string<R: Read>(reader: R) -> Result<String> {
    let mut reader = BufReader::new(reader);
    let mut buf = String::new();
    let mut collected = String::new();

    loop {
        buf.clear();
        let bytes = reader.read_line(&mut buf)?;
        if bytes == 0 {
            break;
        }
        collected.push_str(&buf);
    }

    Ok(collected)
}

fn join_reader(handle: thread::JoinHandle<Result<String>>) -> String {
    match handle.join() {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => {
            log_line("reader-err", &format!("failed to read stream: {}", err));
            String::new()
        }
        Err(_) => {
            log_line("reader-err", "failed to join stream thread");
            String::new()
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    run_batch(&args, &args.task)
}
