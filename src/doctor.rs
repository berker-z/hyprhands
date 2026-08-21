//! Preflight diagnostics.
//!
//! With no install step to lean on, a legible first-run failure *is* the user
//! experience. This checks every binary and environment variable the executor
//! depends on and prints exactly what's missing.

use crate::input::{CLICK_TOOLS, DRAG_TOOLS, SCROLL_TOOLS, TYPE_TOOLS};
use crate::sh;

struct Capability {
    name: &'static str,
    /// Any one of these satisfies it.
    any_of: &'static [&'static str],
    /// Missing-but-optional reports as [--] and does not count as a failure.
    /// For capabilities whose only backend is the last-resort ydotool tier,
    /// which a normal Hyprland setup deliberately goes without.
    optional: bool,
}

const CAPABILITIES: &[Capability] = &[
    Capability {
        name: "window state / focus / cursor / keys",
        any_of: &["hyprctl"],
        optional: false,
    },
    Capability {
        name: "screenshot",
        any_of: &["grim"],
        optional: false,
    },
    Capability {
        name: "click",
        any_of: CLICK_TOOLS,
        optional: false,
    },
    Capability {
        name: "type text",
        any_of: TYPE_TOOLS,
        optional: false,
    },
    Capability {
        name: "scroll",
        any_of: SCROLL_TOOLS,
        optional: false,
    },
    Capability {
        name: "drag (press-move-release)",
        any_of: DRAG_TOOLS,
        optional: true,
    },
];

const ENV_VARS: &[(&str, &str)] = &[
    ("WAYLAND_DISPLAY", "required — the Wayland socket"),
    (
        "HYPRLAND_INSTANCE_SIGNATURE",
        "required — identifies the Hyprland instance",
    ),
    ("XDG_RUNTIME_DIR", "required — where the sockets live"),
];

/// Build the diagnostic report and a failure count.
///
/// Split from [`run`] so the same text can be returned through the MCP
/// `doctor` tool — an agent that hits a missing-capability error can read this
/// itself instead of failing opaquely.
fn build() -> (String, u32) {
    use std::fmt::Write as _;

    let mut out = String::new();
    let mut failures = 0u32;

    let _ = writeln!(out, "hyprhands doctor\n");

    let _ = writeln!(out, "environment");
    for (var, why) in ENV_VARS {
        match std::env::var(var) {
            Ok(val) if !val.is_empty() => {
                let _ = writeln!(out, "  [ok]   {var} = {val}");
            }
            _ => {
                failures += 1;
                let _ = writeln!(out, "  [MISS] {var} — {why}");
            }
        }
    }

    let _ = writeln!(out, "\nbinaries");
    let mut seen: Vec<&str> = Vec::new();
    for cap in CAPABILITIES {
        for tool in cap.any_of {
            if !seen.contains(tool) {
                seen.push(tool);
            }
        }
    }
    for tool in &seen {
        if sh::have(tool) {
            let _ = writeln!(out, "  [ok]   {tool}");
        } else {
            let _ = writeln!(out, "  [--]   {tool} (not installed)");
        }
    }

    let _ = writeln!(out, "\ncapabilities");
    for cap in CAPABILITIES {
        match cap.any_of.iter().find(|t| sh::have(t)) {
            Some(tool) => {
                let _ = writeln!(out, "  [ok]   {:<38} via {tool}", cap.name);
            }
            None if cap.optional => {
                let _ = writeln!(
                    out,
                    "  [--]   {:<38} needs one of: {} (optional)",
                    cap.name,
                    cap.any_of.join(", ")
                );
            }
            None => {
                failures += 1;
                let _ = writeln!(
                    out,
                    "  [MISS] {:<38} needs one of: {}",
                    cap.name,
                    cap.any_of.join(", ")
                );
            }
        }
    }

    let _ = writeln!(out, "\ncompositor");
    match crate::compositor::detect() {
        Ok(comp) => {
            let _ = writeln!(out, "  [ok]   detected {}", comp.name());
            match comp.windows() {
                Ok(windows) => {
                    let _ = writeln!(
                        out,
                        "  [ok]   IPC responding ({} windows open)",
                        windows.len()
                    );
                }
                Err(e) => {
                    failures += 1;
                    let _ = writeln!(out, "  [FAIL] IPC not responding: {e}");
                }
            }
            match comp.monitors() {
                Ok(monitors) => {
                    for m in monitors {
                        let _ = writeln!(
                            out,
                            "  [ok]   {} showing workspace {} ({}x{} @ {})",
                            m.name, m.active_workspace, m.geometry.w, m.geometry.h, m.scale
                        );
                    }
                }
                Err(e) => {
                    failures += 1;
                    let _ = writeln!(out, "  [FAIL] cannot read monitors: {e}");
                }
            }
        }
        Err(e) => {
            failures += 1;
            let _ = writeln!(out, "  [FAIL] {e}");
        }
    }

    let _ = writeln!(out, "\naccessibility (semantic UI tools)");
    match crate::compositor::detect() {
        Ok(comp) => {
            let _ = write!(out, "{}", crate::a11y::doctor_summary(comp.as_ref()));
        }
        Err(_) => {
            let _ = writeln!(out, "  [--]   skipped (no compositor)");
        }
    }

    if failures == 0 {
        let _ = writeln!(out, "\nAll checks passed.");
    } else {
        let _ = writeln!(
            out,
            "\n{failures} problem(s). Missing capabilities degrade gracefully — \
             the tools that depend on them return an error explaining what to \
             install; everything else still works."
        );
    }

    (out, failures)
}

/// Report text, for the MCP `doctor` tool.
pub fn report() -> String {
    build().0
}

/// CLI entry point.
pub fn run() -> i32 {
    let (text, failures) = build();
    print!("{text}");
    if failures == 0 { 0 } else { 1 }
}
