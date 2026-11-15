use crate::manifest::{Check, WarningPolicy};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::LazyLock;

static WARNING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)\b(?:[A-Za-z]+Warning|warning)(?:\s*\[|:)|\b[1-9]\d* warnings?\b")
        .expect("constant warning regex")
});
static NIX_NOTICE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
    r"\Awarning: (?:Git tree '[^\r\n]+' is dirty",
    r"|ignoring untrusted substituter '[^\r\n]+', you are not a trusted user\.",
    r"|ignoring the client-specified setting '[^\r\n]+', because it is a restricted setting and you are not a trusted user",
    r"|[^\r\n]+; retrying in \d+ ms \(attempt \d+/\d+\))\z"
)).expect("constant Nix notice regex")
});
static ENVIRONMENT_FAILURE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"no matching package named|failed to download|can't find crate for|",
        r"required command not found|TOOLCHAIN_(?:STALE|NOT_REGISTERED)|",
        r"Could not find platform independent libraries"
    ))
    .expect("constant environment regex")
});

// The host refused a new thread or process (cgroup pids.max, ulimit -u): the
// check never reached a verdict about the code.
static RESOURCE_EXHAUSTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"failed to create new OS thread|fork: (?:retry: )?Resource temporarily unavailable|",
        r"Resource temporarily unavailable \(os error 11\)|can't start new thread|",
        r"pthread_create failed: Resource temporarily unavailable"
    ))
    .expect("constant resource regex")
});
pub const RESOURCE_HINT: &str = "The host refused a new thread or process (task limit reached, errno EAGAIN). This is an environment blocker, not a code failure: retry when fewer processes run, or raise the task limit of the agent slice.";

pub fn is_warning(text: &str) -> bool {
    WARNING.is_match(text)
}
pub fn is_environment_failure(text: &str) -> bool {
    ENVIRONMENT_FAILURE.is_match(text)
}
pub fn is_resource_exhausted(text: &str) -> bool {
    RESOURCE_EXHAUSTED.is_match(text)
}

// Python splitlines recognizes more separators than Rust str::lines. Exact
// warning declarations retain that behavior, including Unicode separators.
pub fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split([
        '\n', '\r', '\u{b}', '\u{c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}',
        '\u{2029}',
    ])
}

pub fn apply(check: &Check, output: &str, toolchain_hint: &str, result: &mut Value) {
    let mut warnings = BTreeMap::<&str, usize>::new();
    for line in lines(output) {
        if is_warning(line) && !NIX_NOTICE.is_match(line) {
            *warnings.entry(line).or_default() += 1;
        }
    }
    let mut allowed = BTreeMap::<&str, usize>::new();
    let mut extra = String::new();
    if !check.expected_warnings.is_empty() {
        let entries: Vec<Value> = check.expected_warnings.iter().map(|expected| {
            let count = warnings.get(expected.message.as_str()).copied().unwrap_or(0);
            allowed.insert(&expected.message, expected.max_count);
            extra.push_str(&format!("\nExpected diagnostic ({count}/{} occurrences): {}\n", expected.max_count, expected.reason));
            if count > expected.max_count { extra.push_str("Expected diagnostic occurrence limit exceeded.\n"); }
            json!({"message":expected.message,"reason":expected.reason,"max_count":expected.max_count,"count":count})
        }).collect();
        result["expected_warnings"] = json!(entries);
    }
    let returncode = result["returncode"].as_i64();
    if result["output_truncated"] == true || result["timed_out"] == true {
        result["status"] = json!("failed");
    } else if matches!(returncode, Some(126 | 127)) {
        result["status"] = json!("blocked");
        extra.push_str(&format!(
            "\nCommand unavailable (exit {}).\n{toolchain_hint}\n",
            returncode.unwrap_or(127),
        ));
    } else if result["status"] == "failed" && is_resource_exhausted(output) {
        result["status"] = json!("blocked");
        result["resource_exhausted"] = json!(true);
        extra.push_str(&format!("\n{RESOURCE_HINT}\n"));
    } else if result["status"] == "failed" && is_environment_failure(output) {
        result["status"] = json!("blocked");
    } else if result["status"] == "passed" {
        if check.warnings == WarningPolicy::Report {
            if !warnings.is_empty() {
                extra.push_str(&format!(
                    "\n{} warning lines reported; this check declares warnings: report.\n",
                    warnings.values().sum::<usize>()
                ));
            }
        } else if warnings
            .iter()
            .any(|(line, count)| *count > allowed.get(line).copied().unwrap_or(0))
        {
            result["status"] = json!("failed");
            extra.push_str("\nWarnings make this required check unsuccessful. Fix them, declare an exact expected_warnings entry, or set \"warnings\": \"report\" when the tool enforces its own warning policy.\n");
        }
    }
    if result["status"] == "failed"
        && let Some(code) = returncode.filter(|code| *code != 0)
    {
        extra.push_str(&format!("\nCommand exited with status {code}.\n"));
    }
    let existing = result["diagnostics"].as_str().unwrap_or("");
    result["diagnostics"] = json!(format!("{existing}{extra}"));
}
