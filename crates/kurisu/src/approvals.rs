//! Pre-approval rules: which tool calls a session may make without asking.
//!
//! A rule applies to a role (every session with that role) or to one session.
//! It names a tool (`Bash`, `Edit`, a prefix such as `mcp__github__*`, or `*`),
//! and may narrow it:
//!
//! * `in_cwd`: every path the call touches must be inside the session's folder
//!   (for Bash: the shell's cwd, and no `..`, `~` or absolute path outside it);
//! * `command_prefix`: Bash only, the command must start with this text and
//!   contain no shell chaining (`;`, `&&`, `||`, `|`, backticks, `$(`).
//!
//! Deny rules win over allow rules. Anything no rule covers goes to the user.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::store::{Rule, Session};

/// Lexical normalisation (no filesystem access): resolves `.` and `..`.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

fn inside(path: &str, base: &Path, cwd: &Path) -> bool {
    if path.starts_with('~') {
        return false;
    }
    let p = Path::new(path);
    let abs = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    normalize(&abs).starts_with(normalize(base))
}

fn tool_matches(pattern: &str, tool: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => tool.starts_with(prefix),
        None => pattern == tool,
    }
}

const CHAINING: &[&str] = &[";", "&&", "||", "|", "`", "$(", ">", "<", "\n"];

fn bash_in_cwd(cmd: &str, base: &Path, cwd: &Path) -> bool {
    if !inside(&cwd.to_string_lossy(), base, base) {
        return false;
    }
    cmd.split_whitespace().all(|tok| {
        let t = tok.trim_matches(|c| c == '"' || c == '\'');
        if t.contains("..") || t.starts_with('~') || t.contains("$HOME") {
            return false;
        }
        // Any absolute path (also in `--flag=/x`) must stay inside the folder.
        match t.find('/') {
            Some(i) if t[..i].is_empty() || t[..i].ends_with('=') => inside(&t[i..], base, cwd),
            _ => true,
        }
    })
}

/// Does `rule` cover this call by `session`?
pub fn rule_matches(rule: &Rule, session: &Session, tool: &str, input: &Value, call_cwd: &str) -> bool {
    let applies = match rule.scope.as_str() {
        "role" => rule.target == session.role,
        "session" => rule.target == session.id,
        _ => false,
    };
    if !applies || !tool_matches(&rule.tool, tool) {
        return false;
    }
    let base = Path::new(&session.cwd);
    let cwd = if call_cwd.is_empty() { base } else { Path::new(call_cwd) };
    let command = input.get("command").and_then(Value::as_str);
    if let Some(prefix) = &rule.command_prefix {
        let Some(cmd) = command else { return false };
        if !cmd.trim_start().starts_with(prefix.as_str()) || CHAINING.iter().any(|c| cmd.contains(c)) {
            return false;
        }
    }
    if rule.in_cwd {
        if let Some(cmd) = command {
            return bash_in_cwd(cmd, base, cwd);
        }
        let paths: Vec<&str> = ["file_path", "path", "notebook_path"].iter().filter_map(|k| input.get(*k).and_then(Value::as_str)).collect();
        if paths.is_empty() {
            // A path-less tool (e.g. WebFetch) is not "in the folder".
            return false;
        }
        return paths.iter().all(|p| inside(p, base, cwd));
    }
    true
}

/// The deciding rule, if any: deny rules first, then allow rules.
pub fn decide<'a>(rules: &'a [Rule], session: &Session, tool: &str, input: &Value, call_cwd: &str) -> Option<&'a Rule> {
    let hit = |d: &str| rules.iter().find(|r| r.decision == d && rule_matches(r, session, tool, input, call_cwd));
    hit("deny").or_else(|| hit("allow"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session() -> Session {
        Session {
            id: "s1".into(), name: "dev".into(), role: "worker".into(), harness: "claude".into(), cwd: "/w/dev".into(),
            harness_session_id: None, model: "m".into(), effort: None, status: "idle".into(), description: String::new(),
            note: String::new(), last_activity: None, created_by_session: None, created_by_name: None, lifecycle: "active".into(),
        }
    }

    fn rule(scope: &str, target: &str, tool: &str, in_cwd: bool, prefix: Option<&str>, decision: &str) -> Rule {
        Rule {
            id: 1, scope: scope.into(), target: target.into(), target_name: None, tool: tool.into(), in_cwd,
            command_prefix: prefix.map(String::from), decision: decision.into(), created_by: "user".into(), created_at: 0,
        }
    }

    #[test]
    fn bash_in_cwd_for_a_role() {
        let s = session();
        let r = rule("role", "worker", "Bash", true, None, "allow");
        let ok = |cmd: &str, cwd: &str| rule_matches(&r, &s, "Bash", &json!({"command": cmd}), cwd);
        assert!(ok("ls -la", "/w/dev"));
        assert!(ok("cat src/main.rs", "/w/dev/sub"));
        assert!(ok("grep -r foo /w/dev/src", "/w/dev"));
        assert!(!ok("cat /etc/passwd", "/w/dev"));
        assert!(!ok("cat ../other/secret", "/w/dev"));
        assert!(!ok("ls ~", "/w/dev"));
        assert!(!ok("ls", "/w/other"));
        assert!(!ok("cp x --target-directory=/tmp", "/w/dev"));
        // other roles and tools are not covered
        assert!(!rule_matches(&rule("role", "lead", "Bash", true, None, "allow"), &s, "Bash", &json!({"command": "ls"}), "/w/dev"));
        assert!(!rule_matches(&r, &s, "Write", &json!({"file_path": "/w/dev/x"}), "/w/dev"));
    }

    #[test]
    fn paths_prefixes_and_precedence() {
        let s = session();
        let edit = rule("session", "s1", "Edit", true, None, "allow");
        assert!(rule_matches(&edit, &s, "Edit", &json!({"file_path": "/w/dev/a.rs"}), ""));
        assert!(!rule_matches(&edit, &s, "Edit", &json!({"file_path": "/w/dev/../x.rs"}), ""));
        let git = rule("role", "worker", "Bash", false, Some("git status"), "allow");
        assert!(rule_matches(&git, &s, "Bash", &json!({"command": "git status --short"}), ""));
        assert!(!rule_matches(&git, &s, "Bash", &json!({"command": "git status; rm -rf /"}), ""));
        let mcp = rule("role", "worker", "mcp__github__*", false, None, "allow");
        assert!(rule_matches(&mcp, &s, "mcp__github__list_issues", &json!({}), ""));
        let rules = vec![rule("role", "worker", "Bash", false, None, "allow"), rule("session", "s1", "Bash", false, Some("rm"), "deny")];
        assert_eq!(decide(&rules, &s, "Bash", &json!({"command": "rm x"}), "").unwrap().decision, "deny");
        assert_eq!(decide(&rules, &s, "Bash", &json!({"command": "ls"}), "").unwrap().decision, "allow");
        assert!(decide(&rules, &s, "Write", &json!({}), "").is_none());
    }
}
