//! Safe CLI discovery: read a program's `--help`, `--version` or man page.
//!
//! The headless route is the cheapest one when a task does not depend on a
//! live window, but it only helps if the agent knows what the CLI can do.
//! Running an arbitrary binary with `--help` is not automatically safe:
//! single-instance GUI apps forward unknown arguments to the running
//! instance over D-Bus, some ignore `--help` and open a window, and a few
//! hang waiting on a TTY. So every probe here runs
//!
//! - with the display, compositor and session-bus variables removed, so it
//!   cannot open a window or reach a running instance,
//! - with stdin closed, in an empty scratch directory, in its own process
//!   group,
//! - under a hard timeout and an output cap.
//!
//! Results are cached per binary identity (resolved path, size, mtime), so a
//! second session reads the cache instead of re-running anything, and an
//! app update invalidates it automatically.
//!
//! This module only *reads* about a CLI. Running it for real is the agent's
//! own shell's job; hyprhands does not grow a general exec tool.

use crate::action::{Error, Result};
use serde::{Deserialize, Serialize};
use std::io::Read as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// Bytes kept per stream. Help text past this is not worth the tokens.
const CAPTURE_LIMIT: usize = 256 * 1024;
/// Characters returned when no `filter` narrows the text.
const RETURN_LIMIT: usize = 12_000;
/// Lines of context around each filter hit.
const FILTER_CONTEXT: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpSource {
    Help,
    Version,
    Man,
}

impl HelpSource {
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "help" => Ok(HelpSource::Help),
            "version" => Ok(HelpSource::Version),
            "man" => Ok(HelpSource::Man),
            other => Err(Error::new(format!(
                "unknown help source {other:?} (expected help, version, or man)"
            ))),
        }
    }

    fn key(self) -> &'static str {
        match self {
            HelpSource::Help => "help",
            HelpSource::Version => "version",
            HelpSource::Man => "man",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
struct BinaryId {
    path: String,
    size: u64,
    mtime: i64,
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    binary: BinaryId,
    /// The argv that produced `text`, for the report.
    probe: String,
    text: String,
}

/// Resolve a bare command name through PATH, or accept an absolute path.
/// Anything with whitespace or shell syntax is refused: this is a lookup,
/// not a command line.
pub fn resolve(command: &str) -> Result<PathBuf> {
    if command.is_empty()
        || command
            .chars()
            .any(|c| c.is_whitespace() || "|&;<>$`'\"\\*?(){}[]".contains(c))
    {
        return Err(Error::with_hint(
            format!("{command:?} is not a plain program name"),
            "pass just the executable, e.g. `inkscape` or an absolute path; put a \
             subcommand in `subcommand`",
        ));
    }
    if command.starts_with('/') {
        let path = PathBuf::from(command);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(Error::new(format!("{command} does not exist")))
        };
    }
    if command.contains('/') {
        return Err(Error::new(format!(
            "{command:?}: use a bare name (looked up on PATH) or an absolute path"
        )));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(command))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            Error::with_hint(
                format!("`{command}` is not on PATH"),
                "the app's CLI may have a different name than its window class; \
                 app_routes guesses it from the running process",
            )
        })
}

fn identify(path: &Path) -> Option<BinaryId> {
    // Follow symlinks: on Nix `/run/current-system/sw/bin/x` points into the
    // store, and the store path is what changes on update.
    let real = std::fs::canonicalize(path).ok()?;
    let meta = std::fs::metadata(&real).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some(BinaryId {
        path: real.to_string_lossy().into_owned(),
        size: meta.len(),
        mtime,
    })
}

fn cache_path(name: &str, subcommand: Option<&str>, source: HelpSource) -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    let mut key = sanitize(name);
    if let Some(sub) = subcommand {
        key.push('.');
        key.push_str(&sanitize(sub));
    }
    base.join("hyprhands/cli")
        .join(format!("{key}.{}.json", source.key()))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Subcommands are a single token like `export` or `remote-add`.
fn validate_subcommand(sub: &str) -> Result<()> {
    let ok = !sub.is_empty()
        && !sub.starts_with('-')
        && sub
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if ok {
        Ok(())
    } else {
        Err(Error::new(format!(
            "subcommand {sub:?} must be a single word (letters, digits, - _ .)"
        )))
    }
}

struct ProbeOutput {
    text: String,
    success: bool,
    timed_out: bool,
}

/// Run one probe under the isolation described in the module docs.
fn run_probe(program: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<ProbeOutput> {
    let scratch = std::env::temp_dir().join(format!(
        "hyprhands-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir(&scratch)
        .map_err(|e| Error::new(format!("cannot create probe directory: {e}")))?;

    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(&scratch)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for var in [
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "HYPRLAND_INSTANCE_SIGNATURE",
        "DBUS_SESSION_BUS_ADDRESS",
        "AT_SPI_BUS_ADDRESS",
        "LD_LIBRARY_PATH",
    ] {
        cmd.env_remove(var);
    }
    cmd.env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("COLUMNS", "100");
    for (k, v) in env {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn().map_err(|e| {
        let _ = std::fs::remove_dir_all(&scratch);
        Error::new(format!("failed to run {}: {e}", program.display()))
    })?;

    // Readers are detached: a grandchild that inherited the pipe can keep it
    // open after the probe exits, and joining would then hang the server.
    let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut readers = Vec::new();
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let buffer = Arc::clone(&buffer);
        readers.push(std::thread::spawn(move || {
            let mut stream = stream;
            let mut chunk = [0u8; 8192];
            while let Ok(n) = stream.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if let Ok(mut buf) = buffer.lock()
                    && buf.len() < CAPTURE_LIMIT
                {
                    let room = CAPTURE_LIMIT - buf.len();
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }));
    }

    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                kill_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };
    // Give the readers a moment to drain what the process already wrote.
    let drain_deadline = Instant::now() + Duration::from_millis(300);
    while readers.iter().any(|r| !r.is_finished()) && Instant::now() < drain_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    // Anything the probe spawned into its group goes too.
    kill_group(child.id());
    let _ = std::fs::remove_dir_all(&scratch);

    let bytes = buffer.lock().map(|b| b.clone()).unwrap_or_default();
    Ok(ProbeOutput {
        text: clean(&String::from_utf8_lossy(&bytes)),
        success: status.is_some_and(|s| s.success()),
        timed_out,
    })
}

/// Best-effort SIGKILL to a process group, without pulling in libc.
fn kill_group(pgid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Strip ANSI escapes and overstrike (`x\bx`, `_\bx`) formatting.
fn clean(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // CSI: ESC [ ... final byte in @..~
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            '\u{8}' => {
                out.pop();
            }
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

/// Probe or read the cache. Returns (probe description, text, cached).
fn obtain(
    program: &Path,
    id: Option<&BinaryId>,
    subcommand: Option<&str>,
    source: HelpSource,
) -> Result<(String, String, bool)> {
    let name = program
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cache = cache_path(&name, subcommand, source);

    if let (Some(id), Ok(raw)) = (id, std::fs::read_to_string(&cache))
        && let Ok(entry) = serde_json::from_str::<CacheEntry>(&raw)
        && &entry.binary == id
    {
        return Ok((entry.probe, entry.text, true));
    }

    let sub: Vec<&str> = subcommand.into_iter().collect();
    let (probe, text) = match source {
        HelpSource::Man => {
            let page = match subcommand {
                // git-style tools name subcommand pages `git-commit`.
                Some(sub) => format!("{name}-{sub}"),
                None => name.clone(),
            };
            let man = resolve("man").map_err(|_| {
                Error::with_hint(
                    "`man` is not installed",
                    "use source=help instead; most programs describe themselves there",
                )
            })?;
            let out = run_probe(&man, &[&page], &[("MANPAGER", "cat"), ("MANWIDTH", "100")])?;
            if !out.success || out.text.trim().is_empty() {
                return Err(Error::with_hint(
                    format!("no man page for {page}"),
                    "try source=help",
                ));
            }
            (format!("man {page}"), out.text)
        }
        HelpSource::Version => {
            let mut args = sub.clone();
            args.push("--version");
            let out = run_probe(program, &args, &[])?;
            (format!("{name} {}", args.join(" ")), out.text)
        }
        HelpSource::Help => {
            // `--help` first. `-h` is only tried when the program explicitly
            // rejected `--help`: for a few tools `-h` means something else
            // entirely (`shutdown -h` halts), so it is never a blind guess.
            let mut args = sub.clone();
            args.push("--help");
            let mut probe = format!("{name} {}", args.join(" "));
            let mut out = run_probe(program, &args, &[])?;
            if rejected_flag(&out.text) {
                let mut short = sub.clone();
                short.push("-h");
                let retry = run_probe(program, &short, &[])?;
                if retry.text.len() > out.text.len() {
                    probe = format!("{name} {}", short.join(" "));
                    out = retry;
                }
            }
            if out.timed_out && out.text.trim().is_empty() {
                return Err(Error::with_hint(
                    format!(
                        "`{probe}` produced nothing within {}s",
                        PROBE_TIMEOUT.as_secs()
                    ),
                    "it may wait for a display or a TTY. Try source=man, or treat this \
                     app as GUI-only",
                ));
            }
            (probe, out.text)
        }
    };

    if let Some(id) = id
        && !text.trim().is_empty()
        && let Some(dir) = cache.parent()
        && std::fs::create_dir_all(dir).is_ok()
    {
        let entry = CacheEntry {
            binary: id.clone(),
            probe: probe.clone(),
            text: text.clone(),
        };
        if let Ok(json) = serde_json::to_string(&entry) {
            let _ = std::fs::write(&cache, json);
        }
    }
    Ok((probe, text, false))
}

/// Did the program say it does not understand the flag it was given?
fn rejected_flag(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "unrecognized option",
        "unrecognised option",
        "unknown option",
        "invalid option",
        "illegal option",
        "unknown argument",
        "unexpected argument",
    ]
    .iter()
    .any(|m| lower.contains(m))
}

/// Lines containing `needle` (case-insensitive), each with a little context.
fn filter_lines(text: &str, needle: &str) -> (String, usize) {
    let needle = needle.to_lowercase();
    let lines: Vec<&str> = text.lines().collect();
    let hits: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect();
    let mut keep = vec![false; lines.len()];
    for &i in &hits {
        let lo = i.saturating_sub(FILTER_CONTEXT);
        let hi = (i + FILTER_CONTEXT).min(lines.len().saturating_sub(1));
        for k in keep.iter_mut().take(hi + 1).skip(lo) {
            *k = true;
        }
    }
    let mut out = String::new();
    let mut last: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if !keep[i] {
            continue;
        }
        if last.is_some_and(|l| i > l + 1) {
            out.push_str("  ...\n");
        }
        out.push_str(line);
        out.push('\n');
        last = Some(i);
    }
    (out, hits.len())
}

fn truncate(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }
    let mut cut = limit;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    // End on a line boundary so the last option isn't half a sentence.
    let cut = text[..cut].rfind('\n').unwrap_or(cut);
    (text[..cut].to_string(), true)
}

/// The `cli_help` tool.
pub fn help(
    command: &str,
    subcommand: Option<&str>,
    source: HelpSource,
    filter: Option<&str>,
) -> Result<String> {
    if let Some(sub) = subcommand {
        validate_subcommand(sub)?;
    }
    let program = resolve(command)?;
    let id = identify(&program);
    let (probe, text, cached) = obtain(&program, id.as_ref(), subcommand, source)?;

    let origin = match (&id, cached) {
        (Some(id), true) => format!("{} (cached for this exact binary)", id.path),
        (Some(id), false) => id.path.clone(),
        (None, _) => program.display().to_string(),
    };
    let mut out = format!("$ {probe}\nbinary: {origin}\n");
    if text.trim().is_empty() {
        out.push_str(
            "\n(no output — this program does not describe itself this way. Try \
             another source, or treat it as GUI-only.)\n",
        );
        return Ok(out);
    }

    match filter.filter(|f| !f.is_empty()) {
        Some(needle) => {
            let (matched, hits) = filter_lines(&text, needle);
            if hits == 0 {
                out.push_str(&format!(
                    "\nno lines mention {needle:?} ({} lines searched). Try a \
                     shorter word, or omit `filter` to read the start.\n",
                    text.lines().count()
                ));
            } else {
                let (shown, cut) = truncate(&matched, RETURN_LIMIT);
                out.push_str(&format!("\n{hits} line(s) mention {needle:?}:\n\n{shown}"));
                if cut {
                    out.push_str("\n[truncated — use a more specific filter]\n");
                }
            }
        }
        None => {
            let (shown, cut) = truncate(&text, RETURN_LIMIT);
            out.push('\n');
            out.push_str(&shown);
            if cut {
                out.push_str(&format!(
                    "\n[truncated at {RETURN_LIMIT} of {} chars — pass `filter` to \
                     search the rest]\n",
                    text.len()
                ));
            }
        }
    }
    out.push_str(
        "\nThis is documentation, not a verified workflow. Run the command through \
         your own shell; once a CLI route works, record it in app_notes_write.",
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_resolve_and_shell_syntax_is_refused() {
        assert!(resolve("sh").is_ok());
        for bad in ["", "sh -c", "a;b", "$(x)", "rel/path", "x|y"] {
            assert!(resolve(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn subcommands_are_single_words() {
        assert!(validate_subcommand("remote-add").is_ok());
        assert!(validate_subcommand("--rm").is_err());
        assert!(validate_subcommand("a b").is_err());
    }

    #[test]
    fn escapes_and_overstrike_are_removed() {
        assert_eq!(clean("\u{1b}[1mbold\u{1b}[0m"), "bold");
        assert_eq!(clean("N\u{8}NA\u{8}AM\u{8}ME\u{8}E"), "NAME");
        assert_eq!(clean("_\u{8}u"), "u");
        assert_eq!(clean("a\r\nb"), "a\nb");
    }

    #[test]
    fn filter_keeps_context_and_marks_gaps() {
        let text = "zero\none\ntwo\nthree --export\nfour\nfive\nsix\nseven\neight\nnine \
                    --export-type\nten";
        let (out, hits) = filter_lines(text, "EXPORT");
        assert_eq!(hits, 2);
        assert!(out.starts_with("one\ntwo\nthree --export\nfour\nfive\n  ...\n"));
        assert!(out.ends_with("seven\neight\nnine --export-type\nten\n"));
        assert!(!out.contains("zero"));
        assert!(!out.contains("six"));
    }

    #[test]
    fn truncation_ends_on_a_line() {
        let (out, cut) = truncate("aaaa\nbbbb\ncccc", 12);
        assert!(cut);
        assert_eq!(out, "aaaa\nbbbb");
    }

    #[test]
    fn probes_are_isolated_from_the_session() {
        let sh = resolve("sh").unwrap();
        let out = run_probe(
            &sh,
            &[
                "-c",
                "echo \"${WAYLAND_DISPLAY:-none} ${DBUS_SESSION_BUS_ADDRESS:-none}\"",
            ],
            &[],
        )
        .unwrap();
        assert!(out.success);
        assert_eq!(out.text.trim(), "none none");
    }

    #[test]
    fn probes_time_out() {
        let sh = resolve("sh").unwrap();
        let started = Instant::now();
        let out = run_probe(&sh, &["-c", "echo started; sleep 30"], &[]).unwrap();
        assert!(out.timed_out);
        assert!(!out.success);
        assert!(out.text.contains("started"));
        assert!(started.elapsed() < Duration::from_secs(6));
    }
}
