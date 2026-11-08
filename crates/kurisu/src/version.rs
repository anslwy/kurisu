//! Build identity belongs to the server process; clients only format its response.
use serde_json::{json, Value};

pub fn info() -> Value {
    let sha = env!("KURISU_BUILD_COMMIT");
    json!({"version": env!("CARGO_PKG_VERSION"), "git_sha": sha,
        "git_sha_short": sha.chars().take(10).collect::<String>(),
        "dirty": env!("KURISU_BUILD_DIRTY") == "1",
        "built": env!("KURISU_BUILT"), "profile": env!("KURISU_PROFILE"),
        "target": env!("KURISU_TARGET")})
}

pub fn format(v: &Value) -> String {
    let s = |k: &str| v[k].as_str().unwrap_or("unknown");
    let built = s("built").parse::<i64>().ok()
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| s("built").to_string());
    format!("Kurisu {}\nGit {}{} ({})\n{} · {}\nBuilt {}", s("version"), s("git_sha_short"),
        if v["dirty"] == true { " dirty" } else { "" }, s("git_sha"), s("profile"), s("target"), built)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reply_formats_server_fields_in_four_lines() {
        let v = json!({"version":"server-version", "git_sha":"server-sha-full", "git_sha_short":"server-sha",
            "profile":"release", "target":"server-target", "built":"0", "dirty":true});
        let text = format(&v);
        assert_eq!(text, "Kurisu server-version\nGit server-sha dirty (server-sha-full)\nrelease · server-target\nBuilt 1970-01-01T00:00:00Z");
        assert_eq!(text.lines().count(), 4);
    }
}
