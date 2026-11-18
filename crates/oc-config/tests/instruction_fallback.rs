//! `instructions.claude` from the environment (`cli/src/server-process.ts`, 2.3.1 #64474/#64533). Its own test
//! binary, since it sets process environment variables.

#[test]
fn either_claude_code_flag_turns_claude_md_off() {
    let vars = ["OPENCODE_DISABLE_CLAUDE_CODE", "OPENCODE_DISABLE_CLAUDE_CODE_PROMPT", "OPENCODE_DISABLE_CLAUDE_CODE_SKILLS"];
    let set = |values: [Option<&str>; 3]| {
        for (var, value) in vars.iter().zip(values) {
            // SAFETY: the only test in this binary.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(var, value),
                    None => std::env::remove_var(var),
                }
            }
        }
    };
    set([None, None, None]);
    assert!(oc_config::discovery::instructions_claude());
    set([Some("1"), None, None]);
    assert!(!oc_config::discovery::instructions_claude());
    set([None, Some("TRUE"), None]);
    assert!(!oc_config::discovery::instructions_claude());
    set([None, Some("0"), None]);
    assert!(oc_config::discovery::instructions_claude());
    // The skills flag only gates `.claude` skills.
    set([None, None, Some("1")]);
    assert!(oc_config::discovery::instructions_claude());
    assert!(oc_config::discovery::claude_disabled());
    set([None, None, None]);
}
