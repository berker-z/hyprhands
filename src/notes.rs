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
use std::path::PathBuf;

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

/// The running window (if any) whose class matches `app`, case-insensitively.
fn running_fingerprint(comp: &dyn Compositor, app: &str) -> Option<Fingerprint> {
    let windows = comp.windows().ok()?;
    windows
        .iter()
        .find(|w| w.class.eq_ignore_ascii_case(app))
        .and_then(|w| fingerprint_pid(w.pid))
}

fn read_meta(dir: &std::path::Path) -> Option<Meta> {
    let raw = std::fs::read_to_string(dir.join("meta.json")).ok()?;
    serde_json::from_str(&raw).ok()
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
        (Some(stored), Some(current)) if *stored == current => {
            "CURRENT — the running binary is the one these notes were written against.".to_string()
        }
        (Some(stored), Some(current)) => {
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
        (_, None) => format!(
            "UNVERIFIABLE — {app} is not currently running, so the notes cannot \
             be checked against a live binary. Confirm the version once it is."
        ),
        (None, Some(_)) => "UNVERIFIED — these notes were saved while the app was \
             not running, so they carry no version identity."
            .to_string(),
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
    let dir = notes_root().join(sanitize(app));
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::new(format!("cannot create notes dir {}: {e}", dir.display())))?;

    let current = running_fingerprint(comp, app);
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

    let identity = match (&current, &version) {
        (Some(f), Some(v)) => format!("stamped against version {v} ({})", f.exe),
        (Some(f), None) => format!("stamped against {}", f.exe),
        (None, _) => format!(
            "{app} is not running — notes saved without a version stamp; they \
             will read back as UNVERIFIED until rewritten while the app runs"
        ),
    };
    Ok(format!(
        "saved {} chars of notes for {app} — {identity}{}",
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
}
