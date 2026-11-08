//! Human labels for the tool a session is running.
//!
//! ACP agents often send a generic tool title (`shell`, `read`) and keep the
//! useful part - the command line, the file path - only in `raw_input`. The
//! Telegram progress line, the Web UI tool row and `peers_list` all show an
//! activity title, so prefer that real content over the generic label.

use serde_json::Value;

/// Longest activity title we derive. Commands are usually far shorter; this
/// keeps a full command while still bounding the stored/rendered value. The
/// ellipsis in `fit` is only the last-resort guard for longer input.
pub const MAX_TITLE_CHARS: usize = 1000;

/// A specific, one-line title for a running tool call.
///
/// Order of preference:
/// 1. a command line (shell/exec style tools),
/// 2. a file path (read/write/edit style tools),
/// 3. the agent's own title, then the tool name.
pub fn title(tool: &str, title: &str, input: Option<&Value>) -> String {
    let tool = one_line(tool);
    let title = one_line(title);
    let input = input.filter(|v| !v.is_null());
    if let Some(command) = input.and_then(command_text) {
        return fit(&format!("Shell: {command}"));
    }
    if let Some(path) = input.and_then(path_text) {
        return fit(&format!("{}: {path}", file_verb(&tool, &title)));
    }
    if !title.is_empty() {
        fit(&title)
    } else if !tool.is_empty() {
        fit(&tool)
    } else {
        "tool".into()
    }
}

/// A command carried by tool input. ACP tools use `command`; some use an
/// argument vector, and Codex-shaped input is `{"command": ...}` too.
fn command_text(input: &Value) -> Option<String> {
    const KEYS: [&str; 3] = ["command", "cmd", "script"];
    for key in KEYS {
        let Some(value) = input.get(key) else { continue };
        let text = match value {
            Value::String(s) => one_line(s),
            Value::Array(a) => a.iter().filter_map(Value::as_str).map(one_line).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" "),
            _ => String::new(),
        };
        if !text.is_empty() {
            return Some(text);
        }
    }
    // A nested input object (e.g. an MCP call whose arguments hold the command).
    input.get("arguments").and_then(command_text)
}

/// A path carried by tool input. Keep the first one: it is the item the user
/// recognises even when a call touches several files.
fn path_text(input: &Value) -> Option<String> {
    const KEYS: [&str; 6] = ["file_path", "path", "filePath", "file", "filename", "target_file"];
    for key in KEYS {
        if let Some(path) = input.get(key).and_then(Value::as_str).map(one_line).filter(|s| !s.is_empty()) {
            return Some(path);
        }
    }
    if let Some(first) = input.get("paths").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str) {
        let first = one_line(first);
        if !first.is_empty() {
            return Some(first);
        }
    }
    input.get("changes").and_then(Value::as_array).and_then(|a| a.first())
        .and_then(|c| c.get("path")).and_then(Value::as_str).map(one_line).filter(|s| !s.is_empty())
}

/// The verb shown before a path: the tool's own name when it says one.
fn file_verb(tool: &str, title: &str) -> &'static str {
    let label = format!("{tool} {title}").to_ascii_lowercase();
    if ["write", "create", "save", "append"].iter().any(|v| label.contains(*v)) {
        "Write"
    } else if ["edit", "patch", "replace", "update"].iter().any(|v| label.contains(*v)) {
        "Edit"
    } else if ["read", "view", "cat", "open", "fetch"].iter().any(|v| label.contains(*v)) {
        "Read"
    } else if ["list", "glob", "grep", "search", "find"].iter().any(|v| label.contains(*v)) {
        "Search"
    } else {
        "File"
    }
}

/// Collapse every whitespace run (including newlines) into one space.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Keep an activity title to one short line for Telegram and the Web UI.
fn fit(s: &str) -> String {
    if s.chars().count() <= MAX_TITLE_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_TITLE_CHARS - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shell_command_collapses_multiline_whitespace() {
        let input = json!({"command": "cargo test --all\n  --locked\t-- --nocapture"});
        assert_eq!(title("shell", "shell", Some(&input)), "Shell: cargo test --all --locked -- --nocapture");
        assert!(!title("shell", "shell", Some(&input)).contains('\n'));
    }

    #[test]
    fn empty_input_falls_back_to_the_tool_title() {
        assert_eq!(title("shell", "shell", None), "shell");
        assert_eq!(title("shell", "shell", Some(&json!({}))), "shell");
        assert_eq!(title("shell", "shell", Some(&json!({"command": "   "}))), "shell");
    }

    #[test]
    fn three_hundred_char_command_survives_intact() {
        let command = format!("echo {}", "x".repeat(295)); // exactly 300 chars
        let got = title("shell", "shell", Some(&json!({"command": command})));
        assert_eq!(got, format!("Shell: {command}"));
        assert_eq!(got.chars().count(), 307);
    }

    #[test]
    fn very_long_command_is_clamped_with_a_last_resort_ellipsis() {
        let long = format!("echo {}", "x".repeat(5000));
        let input = json!({"command": long});
        let got = title("bash", "bash", Some(&input));
        assert_eq!(got.chars().count(), MAX_TITLE_CHARS);
        assert!(got.ends_with('…'));
        assert!(got.starts_with("Shell: echo xxx"));
    }

    #[test]
    fn file_tools_use_the_path_and_their_own_verb() {
        let read = json!({"file_path": "/tmp/src/main.rs"});
        let edit = json!({"path": "src/lib.rs"});
        let write = json!({"file_path": "src/new.rs"});
        assert_eq!(title("read", "Read", Some(&read)), "Read: /tmp/src/main.rs");
        assert_eq!(title("edit", "Edit", Some(&edit)), "Edit: src/lib.rs");
        assert_eq!(title("write", "Write", Some(&write)), "Write: src/new.rs");
        assert_eq!(title("unknown", "unknown", Some(&json!({"path": "Makefile"}))), "File: Makefile");
    }

    #[test]
    fn argument_vectors_and_nested_arguments_are_commands() {
        assert_eq!(title("bash", "bash", Some(&json!({"command": ["bash", "-lc", "ls -la"]}))), "Shell: bash -lc ls -la");
        assert_eq!(title("mcp", "mcp", Some(&json!({"arguments": {"command": "git status"}}))), "Shell: git status");
    }

    #[test]
    fn non_shell_tools_keep_the_agents_title() {
        assert_eq!(title("mcp", "MCP: kurisu-mcp.session_handoff", Some(&json!({"id": "x"}))), "MCP: kurisu-mcp.session_handoff");
        assert_eq!(title("custom", "Custom work", None), "Custom work");
    }

    #[test]
    fn missing_title_uses_the_tool_name_then_a_generic_label() {
        let input = json!({"command": "rg TODO"});
        assert_eq!(title("bash", "", Some(&input)), "Shell: rg TODO");
        assert_eq!(title("bash", "", None), "bash");
        assert_eq!(title("", "", None), "tool");
    }
}
