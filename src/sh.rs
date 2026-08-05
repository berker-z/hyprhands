//! Thin wrapper over subprocess execution.
//!
//! Everything this tool does is ultimately shelling out to a Wayland utility,
//! so failures here are the common case, not the exception. Error messages
//! carry the command line that failed and, where possible, a remediation hint —
//! the model reads these and can route around a missing binary.

use crate::action::{Error, Result};
use std::process::{Command, Stdio};

/// Is `bin` on PATH?
pub fn have(bin: &str) -> bool {
    // Avoid depending on `which(1)` being installed.
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(bin);
        candidate.is_file() && is_executable(&candidate)
    })
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_p: &std::path::Path) -> bool {
    true
}

fn missing(bin: &str) -> Error {
    Error::with_hint(
        format!("`{bin}` is not on PATH"),
        format!("install {bin}, then re-run `hyprhands doctor` to confirm"),
    )
}

/// Run a command, capture stdout as UTF-8, error on non-zero exit.
pub fn run(bin: &str, args: &[&str]) -> Result<String> {
    let bytes = run_bytes(bin, args)?;
    String::from_utf8(bytes)
        .map_err(|e| Error::new(format!("`{bin}` produced non-UTF-8 output: {e}")))
}

/// Run a command, capture stdout as raw bytes, error on non-zero exit.
pub fn run_bytes(bin: &str, args: &[&str]) -> Result<Vec<u8>> {
    if !have(bin) {
        return Err(missing(bin));
    }

    let output = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::new(format!("failed to spawn `{bin}`: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            format!("exit status {}", output.status)
        } else {
            stderr
        };
        return Err(Error::new(format!(
            "`{bin} {}` failed: {detail}",
            args.join(" ")
        )));
    }

    Ok(output.stdout)
}

/// Run a command purely for its side effect.
pub fn run_ok(bin: &str, args: &[&str]) -> Result<()> {
    run_bytes(bin, args).map(|_| ())
}
