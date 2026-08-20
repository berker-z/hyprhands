//! Input injection.
//!
//! Backend preference, best first:
//!   1. the compositor itself (`hyprctl dispatch`) — needs nothing installed
//!   2. wtype / wlrctl — ordinary Wayland clients, no root, no daemon
//!   3. ydotool — works below the compositor via /dev/uinput, needs a daemon
//!
//! Hyprland implements virtual-keyboard-v1 and wlr-virtual-pointer-v1, so tier
//! 2 is sufficient here; ydotool is the fallback for compositors that don't.

use crate::action::{Button, Error, Point, Result, ScrollDirection};
use crate::compositor::{Compositor, split_chord};
use crate::sh;

/// What each capability would need, for `doctor` and for error hints.
pub const CLICK_TOOLS: &[&str] = &["wlrctl", "ydotool"];
pub const TYPE_TOOLS: &[&str] = &["wtype", "ydotool"];
pub const SCROLL_TOOLS: &[&str] = &["wlrctl", "ydotool"];

fn none_available(what: &str, tools: &[&str]) -> Error {
    Error::with_hint(
        format!("cannot {what}: no supported input tool is installed"),
        format!(
            "install one of: {}. Run `hyprhands doctor` for the full capability list.",
            tools.join(", ")
        ),
    )
}

pub fn move_cursor(comp: &dyn Compositor, to: Point) -> Result<()> {
    // Absolute positioning through the compositor avoids the relative-motion
    // problem that wlr-virtual-pointer has.
    comp.move_cursor(to)
}

pub fn click(comp: &dyn Compositor, at: Option<Point>, button: Button) -> Result<()> {
    if let Some(point) = at {
        move_cursor(comp, point)?;
    }

    if sh::have("wlrctl") {
        return sh::run_ok("wlrctl", &["pointer", "click", button.wlrctl_name()]);
    }
    if sh::have("ydotool") {
        return sh::run_ok("ydotool", &["click", button.ydotool_code()]);
    }
    Err(none_available("click", CLICK_TOOLS))
}

pub fn type_text(text: &str) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }

    if sh::have("wtype") {
        // `--` so text beginning with '-' isn't parsed as a flag.
        return sh::run_ok("wtype", &["--", text]);
    }
    if sh::have("ydotool") {
        return sh::run_ok("ydotool", &["type", "--", text]);
    }
    Err(none_available("type text", TYPE_TOOLS))
}

pub fn key(comp: &dyn Compositor, chord: &str) -> Result<()> {
    // Validate before dispatching so a typo produces a useful message rather
    // than a silent no-op from the compositor.
    let (mods, key) = split_chord(chord)?;

    // A failed compositor dispatch means "try the next backend", the same as
    // Ok(false) — not a fatal error.
    if comp.send_key(chord).unwrap_or(false) {
        return Ok(());
    }

    if sh::have("wtype") {
        let mut argv: Vec<String> = Vec::new();
        for m in &mods {
            argv.push("-M".to_string());
            argv.push(wtype_modifier(m));
        }
        argv.push("-k".to_string());
        argv.push(key.clone());
        // Release modifiers in reverse order.
        for m in mods.iter().rev() {
            argv.push("-m".to_string());
            argv.push(wtype_modifier(m));
        }
        let refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        return sh::run_ok("wtype", &refs);
    }

    Err(none_available(
        &format!("send key chord {chord:?}"),
        TYPE_TOOLS,
    ))
}

/// wtype's name for a Hyprland-normalised modifier.
///
/// wtype accepts shift, capslock, ctrl, logo, win, alt, altgr — it spells
/// "super" as "logo".
fn wtype_modifier(m: &str) -> String {
    match m {
        "SUPER" => "logo".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

pub fn scroll(direction: ScrollDirection, amount: i32) -> Result<()> {
    let (dy, dx) = direction.deltas(amount);

    if sh::have("wlrctl") {
        let (dy, dx) = (dy.to_string(), dx.to_string());
        return sh::run_ok("wlrctl", &["pointer", "scroll", &dy, &dx]);
    }
    if sh::have("ydotool") {
        let (dx, dy) = (dx.to_string(), dy.to_string());
        return sh::run_ok("ydotool", &["mousemove", "-w", "-x", &dx, "-y", &dy]);
    }
    Err(none_available("scroll", SCROLL_TOOLS))
}
