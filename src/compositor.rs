//! Compositor abstraction.
//!
//! Hyprland is the only implementation today, but the trait keeps Sway/river
//! support additive rather than invasive — they expose equivalent JSON IPC,
//! so a second impl is mostly a different argv.

use crate::action::{Error, Point, Rect, Result};
use crate::sh;
use serde::Serialize;
use std::io::Read as _;
use std::time::{Duration, Instant};

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

/// A window the compositor reported as newly mapped after a launch.
#[derive(Debug, Clone)]
pub struct OpenedWindow {
    pub address: String,
    pub workspace: String,
    pub class: String,
    pub title: String,
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

    /// Launch a command and, where the compositor has an event stream, wait up
    /// to `wait` for windows it maps — so callers get an address back instead
    /// of sleeping and re-polling. An empty result after a successful launch
    /// means "nothing mapped in time", not failure: single-instance apps defer
    /// to an existing process, and slow apps map late.
    fn launch_awaited(&self, command: &str, _wait: Duration) -> Result<Vec<OpenedWindow>> {
        self.launch(command)?;
        Ok(Vec::new())
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
            if desktop.is_empty() {
                "unset"
            } else {
                &desktop
            }
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

    /// Does this Hyprland parse `dispatch` arguments as Lua?
    ///
    /// 0.56 replaced the space-separated dispatcher syntax (`movecursor X Y`)
    /// with a Lua expression (`hl.dsp.cursor.move({x=X, y=Y})`); the old form
    /// now fails with exit 7. Detected once per process and cached.
    fn lua_dispatch(&self) -> bool {
        static LUA: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *LUA.get_or_init(|| {
            // Unknown version means newer-than-we-know, so assume Lua.
            self.version()
                .map(|(major, minor)| (major, minor) >= (0, 56))
                .unwrap_or(true)
        })
    }

    /// `(major, minor)` from `hyprctl -j version`, if it can be read.
    fn version(&self) -> Option<(u32, u32)> {
        let value = self.json("version").ok()?;
        let version = value.get("version")?.as_str()?;
        let mut parts = version.trim_start_matches('v').split(['.', '-']);
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        Some((major, minor))
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
            floating: v.get("floating").and_then(|f| f.as_bool()).unwrap_or(false),
            fullscreen: v.get("fullscreen").and_then(|f| f.as_i64()).unwrap_or(0) != 0,
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
        if self.lua_dispatch() {
            let arg = format!("hl.dsp.cursor.move({{x={}, y={}}})", to.x, to.y);
            return self.dispatch(&[&arg]);
        }
        let (x, y) = (to.x.to_string(), to.y.to_string());
        self.dispatch(&["movecursor", &x, &y])
    }

    fn focus_window(&self, address: &str) -> Result<()> {
        if self.lua_dispatch() {
            let arg = format!(
                "hl.dsp.focus({{window=\"address:{}\"}})",
                lua_escape(address)
            );
            return self.dispatch(&[&arg]);
        }
        let target = format!("address:{address}");
        self.dispatch(&["focuswindow", &target])
    }

    fn launch(&self, command: &str) -> Result<()> {
        if self.lua_dispatch() {
            let arg = format!("hl.dsp.exec_cmd(\"{}\")", lua_escape(command));
            return self.dispatch(&[&arg]);
        }
        self.dispatch(&["exec", command])
    }

    fn launch_awaited(&self, command: &str, wait: Duration) -> Result<Vec<OpenedWindow>> {
        // Subscribe BEFORE dispatching, or a fast-mapping window races past us.
        let stream = Hyprland::socket2_path().and_then(|p| {
            let s = std::os::unix::net::UnixStream::connect(p).ok()?;
            s.set_read_timeout(Some(Duration::from_millis(150))).ok()?;
            Some(s)
        });

        self.launch(command)?;

        let Some(mut stream) = stream else {
            return Ok(Vec::new());
        };

        let mut opened: Vec<OpenedWindow> = Vec::new();
        let mut pending = String::new();
        let mut chunk = [0u8; 4096];
        let hard_deadline = Instant::now() + wait;
        // Once something maps, wait only briefly for siblings (splash + main
        // window patterns), then return.
        let mut quiet_deadline: Option<Instant> = None;

        loop {
            let now = Instant::now();
            if now >= hard_deadline || quiet_deadline.is_some_and(|q| now >= q) {
                break;
            }
            match stream.read(&mut chunk) {
                Ok(0) => break, // compositor closed the stream
                Ok(n) => {
                    pending.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    while let Some(nl) = pending.find('\n') {
                        let line: String = pending.drain(..=nl).collect();
                        if let Some(w) = parse_openwindow(line.trim_end()) {
                            opened.push(w);
                            quiet_deadline = Some(Instant::now() + Duration::from_millis(400));
                        }
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => break,
            }
        }
        Ok(opened)
    }

    fn send_key(&self, chord: &str) -> Result<bool> {
        // `sendshortcut` — needs no external input tool, which is why it is
        // tried before wtype/ydotool.
        let (mods, key) = split_chord(chord)?;
        if self.lua_dispatch() {
            let arg = format!(
                "hl.dsp.send_shortcut({{mods=\"{}\", key=\"{}\", window=\"activewindow\"}})",
                lua_escape(&mods.join(" ")),
                lua_escape(&key)
            );
            self.dispatch(&[&arg])?;
        } else {
            let arg = format!("{},{},activewindow", mods.join(" "), key);
            self.dispatch(&["sendshortcut", &arg])?;
        }
        Ok(true)
    }
}

impl Hyprland {
    /// Hyprland's event stream lives next to the command socket.
    fn socket2_path() -> Option<std::path::PathBuf> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
        let sig = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
        Some(
            std::path::Path::new(&runtime)
                .join("hypr")
                .join(sig)
                .join(".socket2.sock"),
        )
    }
}

/// `openwindow>>ADDR,WORKSPACE,CLASS,TITLE` — title may itself contain commas.
///
/// socket2 addresses lack the `0x` prefix that `hyprctl -j` uses everywhere
/// else; it is added here so the result is directly usable as a window handle.
fn parse_openwindow(line: &str) -> Option<OpenedWindow> {
    let data = line.strip_prefix("openwindow>>")?;
    let mut parts = data.splitn(4, ',');
    let address = parts.next()?;
    Some(OpenedWindow {
        address: format!("0x{address}"),
        workspace: parts.next()?.to_string(),
        class: parts.next()?.to_string(),
        title: parts.next().unwrap_or_default().to_string(),
    })
}

/// Escape a string for interpolation into a double-quoted Lua literal.
fn lua_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_split_and_normalise() {
        let (mods, key) = split_chord("ctrl+shift+t").unwrap();
        assert_eq!(mods, vec!["CTRL", "SHIFT"]);
        assert_eq!(key, "t");

        let (mods, key) = split_chord("Super+Return").unwrap();
        assert_eq!(mods, vec!["SUPER"]);
        assert_eq!(key, "Return");

        // A bare key has no modifiers; dashes work as separators too.
        assert_eq!(split_chord("Escape").unwrap(), (vec![], "Escape".into()));
        let (mods, _) = split_chord("ctrl-c").unwrap();
        assert_eq!(mods, vec!["CTRL"]);
    }

    #[test]
    fn chords_reject_junk() {
        assert!(split_chord("").is_err());
        assert!(split_chord("hyper+x").is_err());
    }

    #[test]
    fn openwindow_lines_parse() {
        let w = parse_openwindow("openwindow>>5d55e26dd860,2,org.gnome.Nautilus,Home").unwrap();
        assert_eq!(w.address, "0x5d55e26dd860");
        assert_eq!(w.workspace, "2");
        assert_eq!(w.class, "org.gnome.Nautilus");
        assert_eq!(w.title, "Home");
    }

    #[test]
    fn openwindow_title_keeps_commas() {
        let w = parse_openwindow("openwindow>>abc,1,kitty,fish, vim, and friends").unwrap();
        assert_eq!(w.title, "fish, vim, and friends");
    }

    #[test]
    fn other_events_ignored() {
        assert!(parse_openwindow("activewindow>>kitty,fish").is_none());
        assert!(parse_openwindow("closewindow>>abc").is_none());
        assert!(parse_openwindow("").is_none());
    }

    #[test]
    fn lua_escaping() {
        assert_eq!(lua_escape(r#"say "hi"\now"#), r#"say \"hi\"\\now"#);
        assert_eq!(lua_escape("a\nb"), "a\\nb");
    }
}
