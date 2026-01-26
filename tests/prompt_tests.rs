// Tests for prompt building logic

#[test]
fn test_build_critic_prompt_first_call() {
    let task = "Fix the bug in login";
    let maker_output = "I fixed the bug by changing line 42.";

    let prompt = build_critic_prompt(task, maker_output, false);

    // First call should include full framing
    assert!(prompt.contains("ROLE: CODE REVIEWER"));
    assert!(prompt.contains("## Original Task"));
    assert!(prompt.contains(task));
    assert!(prompt.contains("## Maker's Output"));
    assert!(prompt.contains(maker_output));

    // Should only contain maker_output once
    assert_eq!(prompt.matches(maker_output).count(), 1);
}

#[test]
fn test_build_critic_prompt_continuation() {
    let task = "Fix the bug in login";
    let maker_output = "I made the additional changes you requested.";

    let prompt = build_critic_prompt(task, maker_output, true);

    // Continuation should NOT include full framing
    assert!(!prompt.contains("ROLE: CODE REVIEWER"));
    assert!(!prompt.contains("## Original Task"));
    assert!(!prompt.contains(task)); // task not repeated

    // Should include the new output
    assert!(prompt.contains("The maker has responded"));
    assert!(prompt.contains(maker_output));

    // Should only contain maker_output once
    assert_eq!(prompt.matches(maker_output).count(), 1);
}

// Inline the function for testing (since it's not public)
fn build_critic_prompt(task: &str, maker_output: &str, is_continuation: bool) -> String {
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
        format!(
            r#"ROLE: CODE REVIEWER
You are acting as a CODE REVIEWER. Your job is to evaluate the maker's work for the task below.
Do not offer to do things. Review, comment, critique and guide the maker.

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
