// Tests for prompt building logic

#[test]
fn test_build_maker_prompt_task_only() {
    let task = "Fix the bug in login";
    let prompt = build_maker_prompt(Some(task), None);

    assert!(prompt.contains("## Task"));
    assert!(prompt.contains(task));
    assert!(!prompt.contains("## Context"));
}

#[test]
fn test_build_maker_prompt_context_only() {
    let context = "This is a login system using OAuth";
    let prompt = build_maker_prompt(None, Some(context));

    assert!(prompt.contains("## Context"));
    assert!(prompt.contains(context));
    assert!(!prompt.contains("## Task"));
}

#[test]
fn test_build_maker_prompt_both() {
    let task = "Add OAuth support";
    let context = "This is a login system";
    let prompt = build_maker_prompt(Some(task), Some(context));

    assert!(prompt.contains("## Task"));
    assert!(prompt.contains(task));
    assert!(prompt.contains("## Context"));
    assert!(prompt.contains(context));
}

#[test]
fn test_build_critic_prompt_first_call_with_task() {
    let task = "Fix the bug in login";
    let maker_output = "I fixed the bug by changing line 42.";

    let prompt = build_critic_prompt(Some(task), None, maker_output, false);

    // First call should include full framing
    assert!(prompt.contains("ROLE: Helpful Peer"));
    assert!(prompt.contains("## Original Task"));
    assert!(prompt.contains(task));
    assert!(prompt.contains("## Maker's Output"));
    assert!(prompt.contains(maker_output));

    // Should only contain maker_output once
    assert_eq!(prompt.matches(maker_output).count(), 1);
}

#[test]
fn test_build_critic_prompt_first_call_with_context() {
    let context = "This is a login system using OAuth";
    let maker_output = "I added OAuth support.";

    let prompt = build_critic_prompt(None, Some(context), maker_output, false);

    // First call should include full framing
    assert!(prompt.contains("ROLE: Helpful Peer"));
    assert!(prompt.contains("## Context"));
    assert!(prompt.contains(context));
    assert!(prompt.contains("## Maker's Output"));
    assert!(prompt.contains(maker_output));
    assert!(!prompt.contains("## Original Task"));

    // Should only contain maker_output once
    assert_eq!(prompt.matches(maker_output).count(), 1);
}

#[test]
fn test_build_critic_prompt_first_call_with_both() {
    let task = "Add OAuth support";
    let context = "This is a login system";
    let maker_output = "I added OAuth support.";

    let prompt = build_critic_prompt(Some(task), Some(context), maker_output, false);

    // Should include both sections
    assert!(prompt.contains("ROLE: Helpful Peer"));
    assert!(prompt.contains("## Original Task"));
    assert!(prompt.contains(task));
    assert!(prompt.contains("## Context"));
    assert!(prompt.contains(context));
    assert!(prompt.contains("## Maker's Output"));
    assert!(prompt.contains(maker_output));
}

#[test]
fn test_build_critic_prompt_continuation() {
    let task = "Fix the bug in login";
    let maker_output = "I made the additional changes you requested.";

    let prompt = build_critic_prompt(Some(task), None, maker_output, true);

    // Continuation should NOT include full framing
    assert!(!prompt.contains("ROLE: Helpful Peer"));
    assert!(!prompt.contains("## Original Task"));
    assert!(!prompt.contains(task)); // task not repeated

    // Should include the new output
    assert!(prompt.contains("The maker has responded"));
    assert!(prompt.contains(maker_output));

    // Should only contain maker_output once
    assert_eq!(prompt.matches(maker_output).count(), 1);
}

// Inline the functions for testing (since they're not public)
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
