//! Compositor abstraction.
//!
//! Hyprland is the only implementation today, but the trait is the seam that
//! makes Sway/river support additive rather than invasive — they expose
//! equivalent JSON IPC, so a second impl is mostly a different argv.

use crate::action::{Error, Point, Rect, Result};
use crate::sh;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct WindowInfo {
    /// Compositor handle. Stable for the window's lifetime; pass back to
    /// `focus_window` / screenshot targeting.
    pub address: String,
    pub class: String,
    pub title: String,
    pub workspace: String,
    pub geometry: Rect,
    pub floating: bool,
    pub fullscreen: bool,
    pub focused: bool,
    pub pid: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MonitorInfo {
    pub name: String,
    pub geometry: Rect,
    pub scale: f64,
    pub focused: bool,
    /// Workspace currently displayed on this output.
    ///
    /// Load-bearing: a window whose workspace is not active on *some* monitor
    /// is not being rendered anywhere, so a geometry-based screen capture at
    /// its coordinates silently returns whatever else is painted there.
    /// Hyprland's own `mapped` / `hidden` flags do NOT catch this — an
    /// off-workspace window still reports `mapped: true, hidden: false`.
    pub active_workspace: String,
}

pub trait Compositor {
    fn name(&self) -> &'static str;
    fn windows(&self) -> Result<Vec<WindowInfo>>;
    fn active_window(&self) -> Result<Option<WindowInfo>>;
    fn monitors(&self) -> Result<Vec<MonitorInfo>>;
    fn cursor_position(&self) -> Result<Point>;
    fn move_cursor(&self, to: Point) -> Result<()>;
    fn focus_window(&self, address: &str) -> Result<()>;
    fn launch(&self, command: &str) -> Result<()>;

    /// Send a key chord natively, if the compositor can. Returning `Ok(false)`
    /// means "not supported, fall back to an input tool" — that is not an error.
    fn send_key(&self, _chord: &str) -> Result<bool> {
        Ok(false)
    }

    /// Resolve a window by address, erroring with context if it's gone.
    fn window_by_address(&self, address: &str) -> Result<WindowInfo> {
        let windows = self.windows()?;
        windows
            .iter()
            .find(|w| w.address == address)
            .cloned()
            .ok_or_else(|| {
                let known: Vec<String> = windows
                    .iter()
                    .map(|w| format!("{} ({})", w.address, w.class))
                    .collect();
                Error::with_hint(
                    format!("no window with address {address}"),
                    format!(
                        "it may have closed. currently open: {}",
                        if known.is_empty() {
                            "none".to_string()
                        } else {
                            known.join(", ")
                        }
                    ),
                )
            })
    }
}

/// Pick a compositor from the environment.
pub fn detect() -> Result<Box<dyn Compositor>> {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return Ok(Box::new(Hyprland));
    }

    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    Err(Error::with_hint(
        format!(
            "no supported compositor detected (XDG_CURRENT_DESKTOP={})",
            if desktop.is_empty() { "unset" } else { &desktop }
        ),
        "hyprhands currently supports Hyprland. It must run inside the graphical \
         session so it inherits HYPRLAND_INSTANCE_SIGNATURE, WAYLAND_DISPLAY, \
         and XDG_RUNTIME_DIR — launching it from a bare SSH session or a \
         systemd unit will land here.",
    ))
}

// ---------------------------------------------------------------------------
// Hyprland
// ---------------------------------------------------------------------------

pub struct Hyprland;

impl Hyprland {
    fn json(&self, what: &str) -> Result<serde_json::Value> {
        let out = sh::run("hyprctl", &["-j", what])?;
        serde_json::from_str(&out)
            .map_err(|e| Error::new(format!("could not parse `hyprctl -j {what}` output: {e}")))
    }

    fn dispatch(&self, args: &[&str]) -> Result<()> {
        let mut argv = vec!["dispatch"];
        argv.extend_from_slice(args);
        let out = sh::run("hyprctl", &argv)?;
        // hyprctl exits 0 even for a rejected dispatch; it reports in stdout.
        let trimmed = out.trim();
        if trimmed.eq_ignore_ascii_case("ok") || trimmed.is_empty() {
            Ok(())
        } else {
            Err(Error::new(format!(
                "hyprctl dispatch {} rejected: {trimmed}",
                args.join(" ")
            )))
        }
    }

    fn parse_window(v: &serde_json::Value) -> Option<WindowInfo> {
        let at = v.get("at")?.as_array()?;
        let size = v.get("size")?.as_array()?;
        Some(WindowInfo {
            address: v.get("address")?.as_str()?.to_string(),
            class: v
                .get("class")
                .and_then(|c| c.as_str())
                .unwrap_or_default()
                .to_string(),
            title: v
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            workspace: v
                .get("workspace")
                .and_then(|w| w.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string(),
            geometry: Rect {
                x: at.first()?.as_i64()? as i32,
                y: at.get(1)?.as_i64()? as i32,
                w: size.first()?.as_i64()? as i32,
                h: size.get(1)?.as_i64()? as i32,
            },
            floating: v
                .get("floating")
                .and_then(|f| f.as_bool())
                .unwrap_or(false),
            fullscreen: v
                .get("fullscreen")
                .and_then(|f| f.as_i64())
                .unwrap_or(0)
                != 0,
            // Hyprland orders focus history; 0 is the focused window.
            focused: v
                .get("focusHistoryID")
                .and_then(|f| f.as_i64())
                .map(|id| id == 0)
                .unwrap_or(false),
            pid: v.get("pid").and_then(|p| p.as_i64()).unwrap_or(-1),
        })
    }
}

impl Compositor for Hyprland {
    fn name(&self) -> &'static str {
        "hyprland"
    }

    fn windows(&self) -> Result<Vec<WindowInfo>> {
        let value = self.json("clients")?;
        let arr = value
            .as_array()
            .ok_or_else(|| Error::new("`hyprctl -j clients` did not return an array"))?;
        Ok(arr.iter().filter_map(Hyprland::parse_window).collect())
    }

    fn active_window(&self) -> Result<Option<WindowInfo>> {
        let value = self.json("activewindow")?;
        // Hyprland returns `{}` when nothing is focused.
        if value.get("address").is_none() {
            return Ok(None);
        }
        Ok(Hyprland::parse_window(&value))
    }

    fn monitors(&self) -> Result<Vec<MonitorInfo>> {
        let value = self.json("monitors")?;
        let arr = value
            .as_array()
            .ok_or_else(|| Error::new("`hyprctl -j monitors` did not return an array"))?;
        Ok(arr
            .iter()
            .filter_map(|m| {
                Some(MonitorInfo {
                    name: m.get("name")?.as_str()?.to_string(),
                    geometry: Rect {
                        x: m.get("x")?.as_i64()? as i32,
                        y: m.get("y")?.as_i64()? as i32,
                        w: m.get("width")?.as_i64()? as i32,
                        h: m.get("height")?.as_i64()? as i32,
                    },
                    scale: m.get("scale").and_then(|s| s.as_f64()).unwrap_or(1.0),
                    focused: m.get("focused").and_then(|f| f.as_bool()).unwrap_or(false),
                    active_workspace: m
                        .get("activeWorkspace")
                        .and_then(|w| w.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or_default()
                        .to_string(),
                })
            })
            .collect())
    }

    fn cursor_position(&self) -> Result<Point> {
        let value = self.json("cursorpos")?;
        let x = value
            .get("x")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| Error::new("`hyprctl -j cursorpos` had no x"))?;
        let y = value
            .get("y")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| Error::new("`hyprctl -j cursorpos` had no y"))?;
        Ok(Point {
            x: x as i32,
            y: y as i32,
        })
    }

    fn move_cursor(&self, to: Point) -> Result<()> {
        let (x, y) = (to.x.to_string(), to.y.to_string());
        self.dispatch(&["movecursor", &x, &y])
    }

    fn focus_window(&self, address: &str) -> Result<()> {
        let target = format!("address:{address}");
        self.dispatch(&["focuswindow", &target])
    }

    fn launch(&self, command: &str) -> Result<()> {
        self.dispatch(&["exec", command])
    }

    fn send_key(&self, chord: &str) -> Result<bool> {
        // `sendshortcut MOD,key,window` — needs no external input tool, which
        // is why it is tried before wtype/ydotool.
        let (mods, key) = split_chord(chord)?;
        let arg = format!("{},{},activewindow", mods.join(" "), key);
        self.dispatch(&["sendshortcut", &arg])?;
        Ok(true)
    }
}

/// Split `ctrl+shift+t` into (`["CTRL", "SHIFT"]`, `"t"`).
///
/// Modifier spelling is normalised to Hyprland's; the final key is passed
/// through as an X11 keysym, which is what both Hyprland and wtype expect.
pub fn split_chord(chord: &str) -> Result<(Vec<String>, String)> {
    let parts: Vec<&str> = chord
        .split(['+', '-'])
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();

    let Some((key, mods)) = parts.split_last() else {
        return Err(Error::new(format!("empty key chord {chord:?}")));
    };

    let normalised = mods
        .iter()
        .map(|m| match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Ok("CTRL".to_string()),
            "shift" => Ok("SHIFT".to_string()),
            "alt" | "meta" => Ok("ALT".to_string()),
            "super" | "win" | "cmd" | "mod" => Ok("SUPER".to_string()),
            other => Err(Error::new(format!(
                "unknown modifier {other:?} in {chord:?} (expected ctrl, shift, alt, or super)"
            ))),
        })
        .collect::<Result<Vec<_>>>()?;

    Ok((normalised, (*key).to_string()))
}
