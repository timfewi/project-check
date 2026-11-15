//! Loads a manifest-declared project toolchain so checks use the project's
//! pinned tools rather than whatever happens to be on the caller's `PATH`.
//!
//! `nix print-dev-env --json` evaluates the devShell once per run. Its exported
//! variables are applied to every project check; its `PATH` is placed before
//! the caller's `PATH`, matching `nix develop`. Nothing is passed through a
//! shell: `shellHook` and shell functions are not executed.

use crate::manifest::EnvironmentSpec;
use crate::process::{CaptureFailure, Signals, capture};
use crate::{Error, MAX_OUTPUT_BYTES, Result, util};
use serde_json::{Value, json};
use std::env;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// Variables `nix develop` keeps from the caller, plus per-build bookkeeping
/// that has no meaning outside a builder.
const INHERITED: &[&str] = &[
    "BASHOPTS",
    "HOME",
    "HOST_PATH",
    "NIX_BUILD_TOP",
    "NIX_ENFORCE_PURITY",
    "NIX_LOG_FD",
    "NIX_REMOTE",
    "NIX_SSL_CERT_FILE",
    "OLDPWD",
    "PPID",
    "PWD",
    "SHELL",
    "SHELLOPTS",
    "SHLVL",
    "SSL_CERT_FILE",
    "TEMP",
    "TEMPDIR",
    "TERM",
    "TMP",
    "TMPDIR",
    "TZ",
    "UID",
    "builder",
    "shellHook",
];
/// Search paths the devShell prepends to the caller's value.
const PREPENDED: &[&str] = &["PATH", "XDG_DATA_DIRS"];
const MAX_DIAGNOSTIC_BYTES: usize = 8192;

/// The environment project checks run in.
#[derive(Clone, Debug, Default)]
pub struct Loaded {
    /// Variables set on every project check, in addition to the caller's.
    pub variables: Vec<(String, OsString)>,
    /// Set when the declared environment could not be loaded; checks are blocked.
    pub blocker: Option<String>,
    /// Report entry; `None` when the manifest declares no environment.
    pub report: Option<Value>,
}

impl Loaded {
    /// The caller's environment, unchanged.
    pub fn inherited() -> Self {
        Self::default()
    }

    /// The `PATH` used to resolve check programs.
    pub fn search_path(&self) -> Option<OsString> {
        self.variables
            .iter()
            .find(|(name, _)| name == "PATH")
            .map(|(_, value)| value.clone())
            .or_else(|| env::var_os("PATH"))
    }

    pub fn variable(&self, name: &str) -> Option<OsString> {
        self.variables
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .or_else(|| env::var_os(name))
    }

    pub fn apply(&self, command: &mut Command) {
        for (name, value) in &self.variables {
            command.env(name, value);
        }
    }

    fn blocked(spec: &EnvironmentSpec, started: Instant, diagnostic: String) -> Self {
        let mut report = describe(spec, "blocked", started);
        report["diagnostics"] = json!(diagnostic);
        Self {
            variables: vec![],
            blocker: Some(diagnostic),
            report: Some(report),
        }
    }
}

fn describe(spec: &EnvironmentSpec, status: &str, started: Instant) -> Value {
    json!({
        "kind": "nix-devshell",
        "installable": installable(spec),
        "status": status,
        "duration_seconds": (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0,
    })
}

pub fn installable(spec: &EnvironmentSpec) -> String {
    format!(".#{}", spec.shell)
}

fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if text.len() <= MAX_DIAGNOSTIC_BYTES {
        return text.to_owned();
    }
    let mut start = text.len() - MAX_DIAGNOSTIC_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

/// Loads the declared environment, or returns the caller's when none is declared.
/// Environment problems are reported as a blocker, never as an error, so every
/// check still appears in the report.
pub fn load(
    root: &Path,
    spec: Option<&EnvironmentSpec>,
    nix: &Path,
    signals: &Signals,
) -> Result<Loaded> {
    let Some(spec) = spec else {
        return Ok(Loaded::inherited());
    };
    signals.check()?;
    let started = Instant::now();
    let hint = format!(
        "Fix the project's devShell so `nix develop {}` works, or remove the manifest's environment declaration.",
        installable(spec)
    );
    let program = nix.to_string_lossy();
    if !crate::runner::program_available(&program, root, env::var_os("PATH").as_deref()) {
        return Ok(Loaded::blocked(
            spec,
            started,
            format!("the declared environment needs `nix` on PATH, which is unavailable.\n{hint}"),
        ));
    }
    let mut command = Command::new(nix);
    command
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "print-dev-env",
            "--json",
            "--no-write-lock-file",
            &installable(spec),
        ])
        .current_dir(root);
    let captured = match capture(
        &mut command,
        Duration::from_secs_f64(spec.timeout_seconds),
        MAX_OUTPUT_BYTES,
        signals,
        false,
    ) {
        Ok(captured) => captured,
        Err(error) => {
            return Ok(Loaded::blocked(
                spec,
                started,
                format!("could not start nix: {error}\n{hint}"),
            ));
        }
    };
    let failure = match captured.failure {
        Some(CaptureFailure::Interrupted(signal)) => return Err(Error::interrupted(signal)),
        Some(CaptureFailure::Timeout) => Some(format!(
            "loading the environment timed out after {} seconds; raise environment.timeout_seconds or prebuild the devShell",
            spec.timeout_seconds
        )),
        Some(CaptureFailure::OutputLimit) => Some(format!(
            "the environment description exceeds {MAX_OUTPUT_BYTES} bytes"
        )),
        None if captured.returncode != Some(0) => Some(format!(
            "nix print-dev-env exited with status {}:\n{}",
            captured
                .returncode
                .map_or_else(|| "unknown".into(), |code| code.to_string()),
            tail(&captured.stderr)
        )),
        None => None,
    };
    if let Some(failure) = failure {
        return Ok(Loaded::blocked(spec, started, format!("{failure}\n{hint}")));
    }
    let variables = match util::parse_json(&String::from_utf8_lossy(&captured.output))
        .map_err(|error| error.to_string())
        .and_then(|description| variables(&description))
    {
        Ok(variables) => variables,
        Err(error) => {
            return Ok(Loaded::blocked(
                spec,
                started,
                format!("nix print-dev-env returned an unusable description: {error}\n{hint}"),
            ));
        }
    };
    let mut report = describe(spec, "loaded", started);
    report["variables"] = json!(variables.len());
    Ok(Loaded {
        variables,
        blocker: None,
        report: Some(report),
    })
}

/// Selects the exported string variables of a `nix print-dev-env --json`
/// description and joins search paths with the caller's values.
pub fn variables(description: &Value) -> std::result::Result<Vec<(String, OsString)>, String> {
    let declared = description["variables"]
        .as_object()
        .ok_or("missing variables object")?;
    let outputs: Vec<&str> = declared
        .get("outputs")
        .and_then(|output| output["value"].as_str())
        .map(|value| value.split_whitespace().collect())
        .unwrap_or_default();
    let mut variables = Vec::new();
    for (name, entry) in declared {
        if entry["type"] != "exported"
            || INHERITED.contains(&name.as_str())
            || outputs.contains(&name.as_str())
            || name.is_empty()
            || name.contains(['=', '\0'])
        {
            continue;
        }
        let Some(value) = entry["value"]
            .as_str()
            .filter(|value| !value.contains('\0'))
        else {
            continue;
        };
        let value = if PREPENDED.contains(&name.as_str()) {
            join(value, env::var_os(name).as_deref())
        } else {
            OsString::from(value)
        };
        variables.push((name.clone(), value));
    }
    if !variables.iter().any(|(name, _)| name == "PATH") {
        return Err("the devShell exports no PATH".into());
    }
    Ok(variables)
}

fn join(first: &str, inherited: Option<&OsStr>) -> OsString {
    let mut joined = OsString::from(first);
    if let Some(inherited) = inherited.filter(|value| !value.is_empty()) {
        if !first.is_empty() {
            joined.push(":");
        }
        joined.push(inherited);
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exported_strings_apply_and_search_paths_precede_the_callers() {
        let description = json!({"variables":{
            "PATH":{"type":"exported","value":"/nix/store/synthetic-tool/bin"},
            "PYTHONPATH":{"type":"exported","value":"/nix/store/synthetic-lib"},
            "HOME":{"type":"exported","value":"/homeless-shelter"},
            "TMPDIR":{"type":"exported","value":"/build"},
            "shellHook":{"type":"exported","value":"echo hook"},
            "out":{"type":"exported","value":"/nix/store/synthetic-out"},
            "outputs":{"type":"exported","value":"out"},
            "phases":{"type":"var","value":"buildPhase"},
            "patches":{"type":"array","value":[]},
        }});
        let variables = variables(&description).unwrap();
        let names: Vec<_> = variables.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["PATH", "PYTHONPATH", "outputs"]);
        let path = variables[0].1.to_string_lossy();
        assert!(path.starts_with("/nix/store/synthetic-tool/bin"));
        if let Some(inherited) = env::var_os("PATH").filter(|value| !value.is_empty()) {
            assert!(path.ends_with(&*inherited.to_string_lossy()));
        }
    }

    #[test]
    fn descriptions_without_variables_or_path_are_unusable() {
        assert!(variables(&json!({})).is_err());
        assert!(
            variables(&json!({"variables":{"A":{"type":"exported","value":"1"}}}))
                .unwrap_err()
                .contains("PATH")
        );
    }

    #[test]
    fn long_diagnostics_keep_the_utf8_tail() {
        let text = format!("{}ä final error", "x".repeat(MAX_DIAGNOSTIC_BYTES));
        let kept = tail(text.as_bytes());
        assert!(kept.ends_with("final error"));
        assert!(kept.len() <= MAX_DIAGNOSTIC_BYTES + "…".len());
    }
}
