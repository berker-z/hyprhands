//! Versioned per-application notes — the agent's memory of each program.
//!
//! Driving a GUI app is mostly rediscovery: where the search field is, which
//! menu hides the export button, that the save dialog steals focus. These
//! tools let an agent write down what it learned about an app and read it back
//! next session — *versioned*, because a UI that was true for marcel 0.1.0 is
//! only a hypothesis for marcel 0.1.1.
//!
//! Division of labour: the agent authors the content; the server stamps
//! identity. At write time the running app's binary is fingerprinted (resolved
//! `/proc/<pid>/exe` path + size + mtime — on Nix the store path alone encodes
//! the version). At read time the stored fingerprint is compared against the
//! running app and the notes come back labelled `current`, `stale`, or
//! `unverifiable`, so a model knows whether it is reading facts or hypotheses.
//!
//! Layout: `$XDG_DATA_HOME/hyprhands/notes/<app>/{notes.md, meta.json}`, with
//! superseded versions archived under `history/`.

use crate::action::{Error, Result};
use crate::compositor::Compositor;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Fingerprint {
    pub exe: String,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Serialize, Deserialize)]
struct Meta {
    /// Identity of the binary the notes were written against, if it was
    /// running at write time.
    fingerprint: Option<Fingerprint>,
    /// Optional human-readable version, supplied by the agent or extracted
    /// from a Nix store path.
    version: Option<String>,
    /// Unix seconds of the last write.
    updated: i64,
}

fn notes_root() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("hyprhands/notes")
}

/// One directory per app; the class is user-visible, so keep it readable.
fn sanitize(app: &str) -> String {
    app.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `2026-08-20` from unix seconds (civil-from-days, Howard Hinnant's algorithm).
fn iso_date(unix: i64) -> String {
    let z = unix.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Fingerprint the binary behind a running window's PID.
fn fingerprint_pid(pid: i64) -> Option<Fingerprint> {
    if pid <= 0 {
        return None;
    }
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let meta = std::fs::metadata(&exe).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some(Fingerprint {
        exe: exe.to_string_lossy().into_owned(),
        size: meta.len(),
        mtime,
    })
}

/// Shell-quote one argv fragment for Hyprland's `exec` command string.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Sandboxed harnesses commonly put the MCP server in a PID namespace: the
/// compositor reports host PIDs, but `/proc/<pid>` is absent inside the
/// server. Ask Hyprland to launch a tiny copy of this binary in the graphical
/// session, where that host PID is visible, and exchange only the fingerprint
/// through a private temporary directory.
fn fingerprint_via_compositor(comp: &dyn Compositor, pid: i64) -> Option<Fingerprint> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "hyprhands-fingerprint-{}-{pid}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).ok()?;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    let output = dir.join("result.json");
    let result = (|| {
        let helper = std::env::current_exe().ok()?;
        let command = format!(
            "{} __fingerprint {} {}",
            shell_quote(&helper.to_string_lossy()),
            pid,
            shell_quote(&output.to_string_lossy())
        );
        comp.launch(&command).ok()?;

        for _ in 0..50 {
            if let Ok(raw) = std::fs::read_to_string(&output) {
                return serde_json::from_str(&raw).ok();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    })();
    let _ = std::fs::remove_file(&output);
    let _ = std::fs::remove_dir(&dir);
    result
}

/// Hidden helper entrypoint used by [`fingerprint_via_compositor`]. The output
/// is create-new so even an unexpected path collision cannot overwrite data.
pub fn fingerprint_helper(pid: &str, output: &str) -> i32 {
    let Some(fingerprint) = pid.parse::<i64>().ok().and_then(fingerprint_pid) else {
        return 1;
    };
    let Ok(mut file) = OpenOptions::new().write(true).create_new(true).open(output) else {
        return 1;
    };
    if serde_json::to_writer(&mut file, &fingerprint).is_err() || file.flush().is_err() {
        return 1;
    }
    0
}

/// `/nix/store/<hash>-marcel-0.1.1/bin/marcel` → `0.1.1`.
fn version_from_path(exe: &str) -> Option<String> {
    let store_dir = exe.strip_prefix("/nix/store/")?.split('/').next()?;
    // Past the 32-char hash and its dash: name-version, version starts at the
    // last dash followed by a digit.
    let name_version = store_dir.get(33..)?;
    let idx = name_version
        .char_indices()
        .filter(|&(i, c)| {
            c == '-' && name_version[i + 1..].starts_with(|d: char| d.is_ascii_digit())
        })
        .map(|(i, _)| i)
        .next_back()?;
    Some(name_version[idx + 1..].to_string())
}

enum RunningFingerprint {
    NotRunning,
    Unavailable,
    Found(Fingerprint),
}

/// Binary identities observed while this server process was interacting with
/// apps. Keeping this in memory lets a close-then-checkpoint workflow retain
/// versioning without persisting machine state or trusting model-supplied data.
fn observed_fingerprints() -> &'static Mutex<HashMap<String, Fingerprint>> {
    static OBSERVED: OnceLock<Mutex<HashMap<String, Fingerprint>>> = OnceLock::new();
    OBSERVED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The running window (if any) whose class matches `app`, case-insensitively.
fn running_fingerprint(comp: &dyn Compositor, app: &str) -> RunningFingerprint {
    let Ok(windows) = comp.windows() else {
        return RunningFingerprint::Unavailable;
    };
    let Some(window) = windows.iter().find(|w| w.class.eq_ignore_ascii_case(app)) else {
        return RunningFingerprint::NotRunning;
    };
    fingerprint_pid(window.pid)
        .or_else(|| fingerprint_via_compositor(comp, window.pid))
        .map(RunningFingerprint::Found)
        .unwrap_or(RunningFingerprint::Unavailable)
}

/// Snapshot a live app identity before an interaction that may close it.
pub fn observe_running(comp: &dyn Compositor, app: &str) {
    if let RunningFingerprint::Found(fingerprint) = running_fingerprint(comp, app)
        && let Ok(mut observed) = observed_fingerprints().lock()
    {
        observed.insert(app.to_ascii_lowercase(), fingerprint);
    }
}

fn last_observed(app: &str) -> Option<Fingerprint> {
    observed_fingerprints()
        .lock()
        .ok()
        .and_then(|observed| observed.get(&app.to_ascii_lowercase()).cloned())
}

fn read_meta(dir: &std::path::Path) -> Option<Meta> {
    let raw = std::fs::read_to_string(dir.join("meta.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Whether an app already has a memory record. Used by adapters to prevent a
/// blind full-replacement write before the existing record has been surfaced.
pub fn exists(app: &str) -> bool {
    notes_root().join(sanitize(app)).join("notes.md").is_file()
}

/// Reject unmistakably session-bound diagnostics before they become durable
/// app knowledge. This intentionally targets narrow failure wording rather
/// than broad terms such as "accessibility": stable app-specific setup and
/// workarounds are useful memory, while the state of this machine's bus is not.
fn validate_content(content: &str) -> Result<()> {
    let lower = content.to_ascii_lowercase();
    let transient_markers = [
        "serviceunknown",
        "no_at_bridge",
        "bus unavailable",
        "bus was unavailable",
        "bus not available",
        "bus is unavailable",
        "no accessibility bus",
        "not available in this environment",
        "unavailable in this environment",
        "as of last check",
    ];

    if let Some(marker) = transient_markers
        .iter()
        .find(|marker| lower.contains(**marker))
    {
        return Err(Error::with_hint(
            format!("refusing to save transient session state in app memory (matched {marker:?})"),
            "remove machine/session diagnostics such as current bus or dependency failures; \
             keep only reusable app UI facts, stable app-specific quirks, and verified workflows",
        ));
    }

    Ok(())
}

/// Read notes for one app, or list all apps that have notes.
pub fn read(comp: &dyn Compositor, app: &Option<String>) -> Result<String> {
    let root = notes_root();

    let Some(app) = app else {
        // Listing mode.
        let mut lines = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let dir = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                let meta = read_meta(&dir);
                let (version, updated) = meta
                    .map(|m| {
                        (
                            m.version.unwrap_or_else(|| "unknown".into()),
                            iso_date(m.updated),
                        )
                    })
                    .unwrap_or_default();
                lines.push(format!("- {name} (version {version}, updated {updated})"));
            }
        }
        return Ok(if lines.is_empty() {
            "no app notes saved yet. After you learn how an app's UI works \
             (locations of key controls, quirks, workflows), save it with \
             app_notes_write so the next session starts warm."
                .to_string()
        } else {
            format!("apps with saved notes:\n{}", lines.join("\n"))
        });
    };

    let dir = root.join(sanitize(app));
    let content = std::fs::read_to_string(dir.join("notes.md")).map_err(|_| {
        Error::with_hint(
            format!("no notes saved for {app:?}"),
            "explore the app (ui_tree / screenshot), then record what you learned \
             with app_notes_write — future sessions will thank you",
        )
    })?;
    let meta = read_meta(&dir);

    // Staleness: compare the stored fingerprint with the running binary.
    let status = match (
        &meta.as_ref().and_then(|m| m.fingerprint.clone()),
        running_fingerprint(comp, app),
    ) {
        (Some(stored), RunningFingerprint::Found(current)) if *stored == current => {
            "CURRENT — the running binary is the one these notes were written against.".to_string()
        }
        (Some(stored), RunningFingerprint::Found(current)) => {
            let old_v = meta.as_ref().and_then(|m| m.version.clone());
            let new_v = version_from_path(&current.exe);
            format!(
                "STALE — the app changed since these notes were written \
                 ({} -> {}). Treat every claim below as a hypothesis: verify \
                 against the live UI as you go, and rewrite these notes with \
                 app_notes_write once you know what still holds.",
                old_v.unwrap_or_else(|| stored.exe.clone()),
                new_v.unwrap_or_else(|| current.exe.clone()),
            )
        }
        (_, RunningFingerprint::NotRunning) => format!(
            "UNVERIFIABLE — {app} is not currently running, so the notes cannot \
             be checked against a live binary. Confirm the version once it is."
        ),
        (_, RunningFingerprint::Unavailable) => format!(
            "UNVERIFIABLE — {app} is running, but its executable identity could \
             not be read. Treat the notes as hypotheses and verify against the live UI."
        ),
        (None, RunningFingerprint::Found(_)) => {
            "UNVERIFIED — these notes were saved without a binary \
             identity. Verify them, then rewrite while the app is running."
                .to_string()
        }
    };

    let header = meta
        .map(|m| {
            format!(
                "version: {} | updated: {}",
                m.version.unwrap_or_else(|| "unknown".into()),
                iso_date(m.updated)
            )
        })
        .unwrap_or_default();

    Ok(format!("[{status}]\n{header}\n\n{content}"))
}

/// Write (replace) notes for an app. Superseded versions are archived.
pub fn write(
    comp: &dyn Compositor,
    app: &str,
    content: &str,
    version: Option<&str>,
) -> Result<String> {
    // Validate before creating directories, archiving, or replacing anything.
    validate_content(content)?;

    let dir = notes_root().join(sanitize(app));
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::new(format!("cannot create notes dir {}: {e}", dir.display())))?;

    let running = running_fingerprint(comp, app);
    let live_current = match &running {
        RunningFingerprint::Found(fingerprint) => Some(fingerprint.clone()),
        RunningFingerprint::NotRunning | RunningFingerprint::Unavailable => None,
    };
    let current = live_current.clone().or_else(|| last_observed(app));
    let old_meta = read_meta(&dir);

    // Archive the outgoing notes when they belonged to a different binary.
    let superseded = matches!(
        (&old_meta.as_ref().and_then(|m| m.fingerprint.clone()), &current),
        (Some(old), Some(new)) if old != new
    );
    if superseded && let Ok(old_notes) = std::fs::read_to_string(dir.join("notes.md")) {
        let old = old_meta.as_ref().unwrap();
        let label = old.version.clone().unwrap_or_else(|| iso_date(old.updated));
        let hist = dir.join("history");
        let _ = std::fs::create_dir_all(&hist);
        let _ = std::fs::write(hist.join(format!("{}.md", sanitize(&label))), old_notes);
    }

    let version = version
        .map(|v| v.to_string())
        .or_else(|| current.as_ref().and_then(|f| version_from_path(&f.exe)))
        .or_else(|| {
            old_meta
                .as_ref()
                .and_then(|m| m.version.clone())
                .filter(|_| !superseded)
        });

    let meta = Meta {
        fingerprint: current.clone(),
        version: version.clone(),
        updated: now_unix(),
    };

    std::fs::write(dir.join("notes.md"), content)
        .map_err(|e| Error::new(format!("cannot write notes: {e}")))?;
    std::fs::write(
        dir.join("meta.json"),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .map_err(|e| Error::new(format!("cannot write meta: {e}")))?;

    let observed_suffix = if live_current.is_none() && current.is_some() {
        " (last observed during this session)"
    } else {
        ""
    };
    let identity = match (&current, &version) {
        (Some(f), Some(v)) => {
            format!("stamped against version {v} ({}){observed_suffix}", f.exe)
        }
        (Some(f), None) => format!("stamped against {}{observed_suffix}", f.exe),
        (None, _) => match running {
            RunningFingerprint::NotRunning => format!(
                "{app} is not running — notes saved without a version stamp; they \
                 will read back as UNVERIFIED until rewritten while the app runs"
            ),
            RunningFingerprint::Unavailable => format!(
                "{app} is running, but its executable identity was unavailable — \
                 notes saved without a version stamp"
            ),
            RunningFingerprint::Found(_) => unreachable!(),
        },
    };
    Ok(format!(
        "saved {} chars of notes for {app} — {identity}{}\n\n\
         HYPRHANDS MEMORY RECORD — {app}:\n{content}",
        content.chars().count(),
        if superseded {
            ". Previous version's notes archived to history/"
        } else {
            ""
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nix_store_paths_yield_versions() {
        assert_eq!(
            version_from_path(
                "/nix/store/pbsm8dv4d5nhdmdr494ibwsq393cvka2-kitty-0.48.2/bin/.kitty-wrapped"
            )
            .as_deref(),
            Some("0.48.2")
        );
        // Multi-dash names: the version starts at the last dash-then-digit.
        assert_eq!(
            version_from_path(
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-gnome-calculator-49.1/bin/x"
            )
            .as_deref(),
            Some("49.1")
        );
        assert_eq!(version_from_path("/usr/bin/kitty"), None);
        assert_eq!(
            version_from_path("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-no-version/bin/x"),
            None
        );
    }

    #[test]
    fn dates_render_from_unix_seconds() {
        assert_eq!(iso_date(0), "1970-01-01");
        assert_eq!(iso_date(1787213974), "2026-08-20");
        assert_eq!(iso_date(951782400), "2000-02-29"); // leap day
    }

    #[test]
    fn app_names_sanitise_to_safe_dirs() {
        assert_eq!(sanitize("org.gnome.Nautilus"), "org.gnome.nautilus");
        assert_eq!(
            sanitize("io.github.berker_z.Marcel"),
            "io.github.berker_z.marcel"
        );
        assert_eq!(sanitize("weird/../app name"), "weird_.._app_name");
    }

    #[test]
    fn current_process_can_be_fingerprinted_directly() {
        let fingerprint = fingerprint_pid(std::process::id().into()).unwrap();
        assert!(!fingerprint.exe.is_empty());
        assert!(fingerprint.size > 0);
    }

    #[test]
    fn helper_commands_shell_quote_paths() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's here"), "'it'\\''s here'");
    }

    #[test]
    fn transient_environment_failures_are_rejected_from_memory() {
        for content in [
            "AT-SPI bus was unavailable in this session",
            "Verification methods (no accessibility bus)",
            "ui_tree failed with ServiceUnknown",
            "NO_AT_BRIDGE=1 was set",
            "This was true as of last check",
        ] {
            let error = validate_content(content).unwrap_err();
            assert!(error.message.contains("transient session state"));
        }
    }

    #[test]
    fn stable_app_specific_accessibility_workarounds_are_allowed() {
        validate_content(
            "Electron apps launched with --force-renderer-accessibility expose semantic controls.",
        )
        .unwrap();
        validate_content("The Next control is immediately right of Play/Pause.").unwrap();
    }
}
