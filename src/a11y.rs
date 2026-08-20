//! AT-SPI accessibility backend: semantic UI targeting.
//!
//! Screenshots + pixel coordinates are the fallback of computer use, not the
//! ideal. Where an app exposes an AT-SPI tree, an agent can *read* the UI
//! (roles, names, states, editable text) and *act* on it (invoke an element's
//! own "click" action, set a text field's contents) without any screenshot,
//! any pixel math, or any input tool installed.
//!
//! ## Coordinates on Wayland
//!
//! Under Wayland an app cannot know its own absolute position, so AT-SPI
//! screen coordinates are unreliable. But *window-relative* extents are fine —
//! and the compositor knows exactly where each window is. So: extents are
//! fetched with `ATSPI_COORD_TYPE_WINDOW`, normalised against the toplevel
//! frame's own origin (which cancels out client-side-decoration shadow
//! offsets), and added to the window geometry Hyprland reports. The result is
//! an absolute layout coordinate that `click` and `move_cursor` accept as-is.
//!
//! ## Wiring apps to windows
//!
//! An AT-SPI application is matched to a compositor window by PID (asking the
//! a11y bus's own daemon for each peer's process id), and a toplevel frame to
//! a specific window by title. Both are heuristics; both are reported rather
//! than silently guessed.

use crate::action::{Error, Rect, Result};
use crate::compositor::{Compositor, WindowInfo};
use std::fmt::Write as _;
use zbus::blocking::Connection;
use zbus::zvariant::OwnedObjectPath;

const REGISTRY: &str = "org.a11y.atspi.Registry";
const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";
const PATH_PREFIX: &str = "/org/a11y/atspi/accessible/";
const IFACE_ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const IFACE_COMPONENT: &str = "org.a11y.atspi.Component";
const IFACE_ACTION: &str = "org.a11y.atspi.Action";
const IFACE_TEXT: &str = "org.a11y.atspi.Text";
const IFACE_EDITABLE: &str = "org.a11y.atspi.EditableText";
const IFACE_VALUE: &str = "org.a11y.atspi.Value";

const COORD_WINDOW: u32 = 1;
const COORD_SCREEN: u32 = 0;

// AT-SPI state bits (org.a11y.atspi.State enum values).
const STATE_CHECKED: u64 = 4;
const STATE_EDITABLE: u64 = 7;
const STATE_ENABLED: u64 = 8;
const STATE_EXPANDED: u64 = 10;
const STATE_FOCUSED: u64 = 12;
const STATE_SELECTED: u64 = 23;
const STATE_SENSITIVE: u64 = 24;
const STATE_SHOWING: u64 = 25;

/// AT-SPI role names by enum value, for peers that lack `GetRoleName`.
/// Matches at-spi2-core's own table (`atspi_role_get_name`).
const ROLE_NAMES: &[&str] = &[
    "invalid",
    "accelerator label",
    "alert",
    "animation",
    "arrow",
    "calendar",
    "canvas",
    "check box",
    "check menu item",
    "color chooser",
    "column header",
    "combo box",
    "date editor",
    "desktop icon",
    "desktop frame",
    "dial",
    "dialog",
    "directory pane",
    "drawing area",
    "file chooser",
    "filler",
    "focus traversable",
    "font chooser",
    "frame",
    "glass pane",
    "html container",
    "icon",
    "image",
    "internal frame",
    "label",
    "layered pane",
    "list",
    "list item",
    "menu",
    "menu bar",
    "menu item",
    "option pane",
    "page tab",
    "page tab list",
    "panel",
    "password text",
    "popup menu",
    "progress bar",
    "push button",
    "radio button",
    "radio menu item",
    "root pane",
    "row header",
    "scroll bar",
    "scroll pane",
    "separator",
    "slider",
    "spin button",
    "split pane",
    "status bar",
    "table",
    "table cell",
    "table column header",
    "table row header",
    "tearoff menu item",
    "terminal",
    "text",
    "toggle button",
    "tool bar",
    "tool tip",
    "tree",
    "tree table",
    "unknown",
    "viewport",
    "window",
    "extended",
    "header",
    "footer",
    "paragraph",
    "ruler",
    "application",
    "autocomplete",
    "editbar",
    "embedded",
    "entry",
    "chart",
    "caption",
    "document frame",
    "heading",
    "page",
    "section",
    "redundant object",
    "form",
    "link",
    "input method window",
    "table row",
    "tree item",
    "document spreadsheet",
    "document presentation",
    "document text",
    "document web",
    "document email",
    "comment",
    "list box",
    "grouping",
    "image map",
    "notification",
    "info bar",
    "level bar",
    "title bar",
    "block quote",
    "audio",
    "video",
    "definition",
    "article",
    "landmark",
    "log",
    "marquee",
    "math",
    "rating",
    "timer",
    "static",
    "math fraction",
    "math root",
    "subscript",
    "superscript",
    "description list",
    "description term",
    "description value",
    "footnote",
    "content deletion",
    "content insertion",
    "mark",
    "suggestion",
    "push button menu",
];

/// Traversal ceilings. A browser tab can expose tens of thousands of nodes;
/// past this budget the walk reports truncation instead of stalling the turn.
const NODE_BUDGET: usize = 1500;
const DEFAULT_DEPTH: u32 = 12;

const UNAVAILABLE_HINT: &str = "\
The accessibility bus is how apps expose their UI semantically. To enable it: \
NixOS: `services.gnome.at-spi2-core.enable = true` (and ensure NO_AT_BRIDGE \
is not set to 1); other distros: install at-spi2-core. Per-toolkit: Qt apps \
need QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1, Electron apps need \
--force-renderer-accessibility, GTK apps join automatically. Apps only \
register at startup, so restart them after enabling. Screenshot-based tools \
keep working without any of this.";

pub struct A11y {
    conn: Connection,
}

/// Is the accessibility bus reachable right now? Cheap (one session-bus call),
/// deliberately uncached — the bus can come up mid-session.
pub fn bus_available() -> bool {
    let Ok(session) = Connection::session() else {
        return false;
    };
    session
        .call_method(
            Some("org.a11y.Bus"),
            "/org/a11y/bus",
            Some("org.a11y.Bus"),
            "GetAddress",
            &(),
        )
        .is_ok()
}

/// Advertise an assistive technology on `org.a11y.Status` (best-effort).
///
/// Chromium, Electron and Firefox ignore the env-var route entirely; what they
/// watch is the a11y bus's ScreenReaderEnabled flag, enabling their
/// accessibility bridges when it turns on — the same signal Orca uses. Setting
/// it costs nothing for apps that don't care.
pub fn advertise_screen_reader() -> bool {
    let Ok(session) = Connection::session() else {
        return false;
    };
    let mut ok = false;
    for prop in ["IsEnabled", "ScreenReaderEnabled"] {
        ok |= session
            .call_method(
                Some("org.a11y.Bus"),
                "/org/a11y/bus",
                Some("org.freedesktop.DBus.Properties"),
                "Set",
                &("org.a11y.Status", prop, zbus::zvariant::Value::from(true)),
            )
            .is_ok();
    }
    ok
}

/// Wrap a launch command so the child starts with accessibility on.
///
/// Returns the (possibly rewritten) command and whether injection happened.
/// The whole command is run through `sh -c` so compound commands keep their
/// meaning under the `env` prefix. When the a11y bus is down the command is
/// passed through untouched — half-enabled accessibility helps nobody.
pub fn accessible_command(command: &str) -> (String, bool) {
    if !bus_available() {
        return (command.to_string(), false);
    }
    advertise_screen_reader();
    let escaped = command.replace('\'', "'\\''");
    (
        format!("env -u NO_AT_BRIDGE QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 sh -c '{escaped}'"),
        true,
    )
}

/// `(bus name, object path)` — how AT-SPI references an accessible object.
type ARef = (String, OwnedObjectPath);

/// What one node looks like, resolved.
struct Node {
    id: String,
    role: String,
    name: String,
    states: Vec<&'static str>,
    /// Absolute layout rect, when resolvable and showing.
    rect: Option<Rect>,
    actions: Vec<String>,
    editable: bool,
    has_text: bool,
    value: Option<f64>,
}

impl A11y {
    /// Connect to the accessibility bus, or explain why that's impossible.
    pub fn connect() -> Result<A11y> {
        // Standard override honoured by libatspi; also handy for testing.
        let address = match std::env::var("AT_SPI_BUS_ADDRESS") {
            Ok(a) if !a.is_empty() => a,
            _ => {
                let session = Connection::session().map_err(|e| {
                    Error::new(format!("cannot connect to the D-Bus session bus: {e}"))
                })?;
                let msg = session
                    .call_method(
                        Some("org.a11y.Bus"),
                        "/org/a11y/bus",
                        Some("org.a11y.Bus"),
                        "GetAddress",
                        &(),
                    )
                    .map_err(|e| {
                        Error::with_hint(
                            format!("AT-SPI accessibility bus is not available: {e}"),
                            UNAVAILABLE_HINT,
                        )
                    })?;
                msg.body()
                    .deserialize::<String>()
                    .map_err(|e| Error::new(format!("could not read a11y bus address: {e}")))?
            }
        };

        let conn = zbus::blocking::connection::Builder::address(address.as_str())
            .and_then(|b| b.build())
            .map_err(|e| {
                Error::with_hint(
                    format!("could not connect to a11y bus at {address}: {e}"),
                    UNAVAILABLE_HINT,
                )
            })?;

        let a11y = A11y { conn };

        // Present as an assistive technology, not a drive-by client. Chromium
        // and friends hydrate their full trees only while at least one event
        // listener is registered with the registry from a *live* connection —
        // ours persists for the length of the session, so this registration
        // holds until the server exits. Best-effort: GTK/Qt don't need it.
        for event in ["object:state-changed", "window:activate"] {
            let _ = a11y.call::<()>(
                REGISTRY,
                "/org/a11y/atspi/registry",
                "org.a11y.atspi.Registry",
                "RegisterEvent",
                &(event,),
            );
        }

        Ok(a11y)
    }

    // -- low-level D-Bus helpers --------------------------------------------

    fn call<T>(
        &self,
        dest: &str,
        path: &str,
        iface: &str,
        method: &str,
        body: &(impl serde::Serialize + zbus::zvariant::DynamicType),
    ) -> Result<T>
    where
        T: for<'de> serde::Deserialize<'de> + zbus::zvariant::Type,
    {
        let msg = self
            .conn
            .call_method(Some(dest), path, Some(iface), method, body)
            .map_err(|e| {
                Error::new(format!(
                    "a11y call {iface}.{method} on {dest}{path} failed: {e}"
                ))
            })?;
        msg.body().deserialize::<T>().map_err(|e| {
            Error::new(format!(
                "a11y reply {iface}.{method}: unexpected shape: {e}"
            ))
        })
    }

    fn property_str(&self, r: &ARef, iface: &str, prop: &str) -> Result<String> {
        let v: (zbus::zvariant::OwnedValue,) = self.call(
            &r.0,
            r.1.as_str(),
            "org.freedesktop.DBus.Properties",
            "Get",
            &(iface, prop),
        )?;
        String::try_from(v.0)
            .map_err(|e| Error::new(format!("property {prop} was not a string: {e}")))
    }

    fn property_i32(&self, r: &ARef, iface: &str, prop: &str) -> Result<i32> {
        let v: (zbus::zvariant::OwnedValue,) = self.call(
            &r.0,
            r.1.as_str(),
            "org.freedesktop.DBus.Properties",
            "Get",
            &(iface, prop),
        )?;
        i32::try_from(v.0).map_err(|e| Error::new(format!("property {prop} was not an i32: {e}")))
    }

    fn property_f64(&self, r: &ARef, iface: &str, prop: &str) -> Result<f64> {
        let v: (zbus::zvariant::OwnedValue,) = self.call(
            &r.0,
            r.1.as_str(),
            "org.freedesktop.DBus.Properties",
            "Get",
            &(iface, prop),
        )?;
        f64::try_from(v.0).map_err(|e| Error::new(format!("property {prop} was not an f64: {e}")))
    }

    // -- AT-SPI object queries ----------------------------------------------

    fn children(&self, r: &ARef) -> Result<Vec<ARef>> {
        let (refs,): (Vec<ARef>,) =
            self.call(&r.0, r.1.as_str(), IFACE_ACCESSIBLE, "GetChildren", &())?;
        Ok(refs)
    }

    fn role_name(&self, r: &ARef) -> Result<String> {
        // AccessKit-based apps (GPUI/Zed, egui, Slint) implement the numeric
        // GetRole but not GetRoleName — fall back to the standard role table.
        match self.call::<(String,)>(&r.0, r.1.as_str(), IFACE_ACCESSIBLE, "GetRoleName", &()) {
            Ok((s,)) => Ok(s),
            Err(_) => {
                let (n,): (u32,) =
                    self.call(&r.0, r.1.as_str(), IFACE_ACCESSIBLE, "GetRole", &())?;
                Ok(ROLE_NAMES
                    .get(n as usize)
                    .copied()
                    .unwrap_or("unknown")
                    .to_string())
            }
        }
    }

    fn name(&self, r: &ARef) -> String {
        self.property_str(r, IFACE_ACCESSIBLE, "Name")
            .unwrap_or_default()
    }

    fn interfaces(&self, r: &ARef) -> Vec<String> {
        self.call::<(Vec<String>,)>(&r.0, r.1.as_str(), IFACE_ACCESSIBLE, "GetInterfaces", &())
            .map(|t| t.0)
            .unwrap_or_default()
    }

    fn state_bits(&self, r: &ARef) -> u64 {
        let words: Vec<u32> = self
            .call::<(Vec<u32>,)>(&r.0, r.1.as_str(), IFACE_ACCESSIBLE, "GetState", &())
            .map(|t| t.0)
            .unwrap_or_default();
        let lo = words.first().copied().unwrap_or(0) as u64;
        let hi = words.get(1).copied().unwrap_or(0) as u64;
        lo | (hi << 32)
    }

    fn extents(&self, r: &ARef, coord_type: u32) -> Result<Rect> {
        let ((x, y, w, h),): ((i32, i32, i32, i32),) = self.call(
            &r.0,
            r.1.as_str(),
            IFACE_COMPONENT,
            "GetExtents",
            &(coord_type,),
        )?;
        Ok(Rect { x, y, w, h })
    }

    fn action_names(&self, r: &ARef) -> Vec<String> {
        let n = self.property_i32(r, IFACE_ACTION, "NActions").unwrap_or(0);
        (0..n.min(8))
            .filter_map(|i| {
                self.call::<(String,)>(&r.0, r.1.as_str(), IFACE_ACTION, "GetName", &(i,))
                    .ok()
                    .map(|t| t.0)
            })
            .collect()
    }

    /// PID of the process behind a bus connection, asked of the a11y bus daemon.
    fn peer_pid(&self, bus_name: &str) -> Option<u32> {
        self.call::<(u32,)>(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetConnectionUnixProcessID",
            &(bus_name,),
        )
        .map(|t| t.0)
        .ok()
    }

    /// All registered applications: `(ref, pid)`.
    pub fn applications(&self) -> Result<Vec<(ARef, Option<u32>)>> {
        let root: ARef = (
            REGISTRY.to_string(),
            OwnedObjectPath::try_from(ROOT_PATH).unwrap(),
        );
        let apps = self.children(&root)?;
        Ok(apps
            .into_iter()
            .map(|r| {
                let pid = self.peer_pid(&r.0);
                (r, pid)
            })
            .collect())
    }

    // -- element ids ---------------------------------------------------------

    fn element_id(r: &ARef) -> String {
        let tail =
            r.1.as_str()
                .strip_prefix(PATH_PREFIX)
                .unwrap_or(r.1.as_str());
        format!("{}|{}", r.0, tail)
    }

    fn parse_element_id(id: &str) -> Result<ARef> {
        let (bus, tail) = id.split_once('|').ok_or_else(|| {
            Error::with_hint(
                format!("malformed element id {id:?}"),
                "element ids come from ui_tree / find_element and look like ':1.42|216'",
            )
        })?;
        let path = if tail.starts_with('/') {
            tail.to_string()
        } else {
            format!("{PATH_PREFIX}{tail}")
        };
        let path = OwnedObjectPath::try_from(path.as_str())
            .map_err(|e| Error::new(format!("element id {id:?} has an invalid path: {e}")))?;
        Ok((bus.to_string(), path))
    }

    // -- window mapping ------------------------------------------------------

    /// The AT-SPI application for a compositor window, by PID.
    fn app_for_window(&self, window: &WindowInfo) -> Result<ARef> {
        let apps = self.applications()?;
        let total = apps.len();
        apps.into_iter()
            .find(|(_, pid)| pid.map(|p| p as i64) == Some(window.pid))
            .map(|(r, _)| r)
            .ok_or_else(|| {
                Error::with_hint(
                    format!(
                        "{} (pid {}) is not registered on the accessibility bus \
                         ({total} app(s) are)",
                        window.class, window.pid
                    ),
                    "the app does not expose AT-SPI. Electron apps need \
                     --force-renderer-accessibility, Qt apps need \
                     QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1, and apps only register at \
                     startup — restart after setting. Fall back to screenshot + \
                     click for this window.",
                )
            })
    }

    /// The toplevel frame inside `app` that corresponds to `window`, plus the
    /// origin to subtract from window-relative extents (the frame's own
    /// reported origin, which absorbs CSD shadow offsets).
    fn frame_for_window(&self, app: &ARef, window: &WindowInfo) -> Result<(ARef, Rect)> {
        let frames = self.children(app)?;
        if frames.is_empty() {
            return Err(Error::new(format!(
                "{} exposes an accessible application with no toplevels",
                window.class
            )));
        }

        let chosen = if frames.len() == 1 {
            frames.into_iter().next().unwrap()
        } else {
            let mut named: Vec<(ARef, String)> = frames
                .into_iter()
                .map(|f| {
                    let n = self.name(&f);
                    (f, n)
                })
                .collect();
            let title = &window.title;
            let pick = named
                .iter()
                .position(|(_, n)| n == title)
                .or_else(|| {
                    named.iter().position(|(_, n)| {
                        !n.is_empty() && (title.contains(n.as_str()) || n.contains(title.as_str()))
                    })
                })
                .unwrap_or(0);
            named.swap_remove(pick).0
        };

        let origin = self.extents(&chosen, COORD_WINDOW).unwrap_or(Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        });
        Ok((chosen, origin))
    }

    /// Resolve one node's interesting facts. `offset` maps window-relative
    /// coordinates to absolute layout ones; `None` means coords are unknowable.
    fn resolve(&self, r: &ARef, offset: Option<(Rect, Rect)>) -> Node {
        let role = self.role_name(r).unwrap_or_else(|_| "?".to_string());
        let name = self.name(r);
        let bits = self.state_bits(r);
        let ifaces = self.interfaces(r);
        let has = |i: &str| ifaces.iter().any(|s| s == i);

        let mut states = Vec::new();
        if bits & (1 << STATE_FOCUSED) != 0 {
            states.push("focused");
        }
        if bits & (1 << STATE_EDITABLE) != 0 {
            states.push("editable");
        }
        if bits & (1 << STATE_CHECKED) != 0 {
            states.push("checked");
        }
        if bits & (1 << STATE_SELECTED) != 0 {
            states.push("selected");
        }
        if bits & (1 << STATE_EXPANDED) != 0 {
            states.push("expanded");
        }
        // GTK4 sets SENSITIVE but not ENABLED; Qt sets ENABLED. Only the
        // absence of both means the control is actually inert.
        if bits & ((1 << STATE_ENABLED) | (1 << STATE_SENSITIVE)) == 0 {
            states.push("disabled");
        }
        let showing = bits & (1 << STATE_SHOWING) != 0;
        if !showing {
            states.push("not-showing");
        }

        let rect = if has(IFACE_COMPONENT) && showing {
            match offset {
                Some((win, frame_origin)) => self.extents(r, COORD_WINDOW).ok().map(|e| Rect {
                    x: win.x + e.x - frame_origin.x,
                    y: win.y + e.y - frame_origin.y,
                    w: e.w,
                    h: e.h,
                }),
                // No window mapping — raw screen coords are better than nothing
                // (correct for XWayland clients, unreliable for native Wayland).
                None => self.extents(r, COORD_SCREEN).ok(),
            }
            .filter(|r| r.w > 0 && r.h > 0)
        } else {
            None
        };

        let actions = if has(IFACE_ACTION) {
            self.action_names(r)
        } else {
            Vec::new()
        };
        let value = if has(IFACE_VALUE) {
            self.property_f64(r, IFACE_VALUE, "CurrentValue").ok()
        } else {
            None
        };

        Node {
            id: Self::element_id(r),
            role,
            name,
            states,
            rect,
            actions,
            editable: has(IFACE_EDITABLE),
            has_text: has(IFACE_TEXT),
            value,
        }
    }
}

/// Container roles that add nesting but no information; they are traversed
/// but not printed unless they carry a name, an action, or text.
fn is_boring(node: &Node) -> bool {
    matches!(
        node.role.as_str(),
        "panel"
            | "filler"
            | "section"
            | "viewport"
            | "scroll pane"
            | "split pane"
            | "box"
            | "separator"
            | "redundant object"
            | "unknown"
            | "generic"
            | "group"
    ) && node.name.is_empty()
        && node.actions.is_empty()
        && !node.editable
        && node.value.is_none()
}

fn render(node: &Node, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    let _ = write!(out, "{indent}{}", node.role);
    if !node.name.is_empty() {
        let name: String = node.name.chars().take(80).collect();
        let _ = write!(out, " \"{name}\"");
    }
    if let Some(r) = &node.rect {
        let _ = write!(
            out,
            " ({},{} {}x{})",
            r.x + r.w / 2,
            r.y + r.h / 2,
            r.w,
            r.h
        );
    }
    if let Some(v) = node.value {
        let _ = write!(out, " value={v}");
    }
    if !node.states.is_empty() {
        let _ = write!(out, " [{}]", node.states.join(","));
    }
    if !node.actions.is_empty() {
        let _ = write!(out, " actions:{}", node.actions.join(","));
    }
    if node.editable {
        let _ = write!(out, " {{set_text}}");
    }
    let _ = writeln!(out, "  «{}»", node.id);
}

struct Walk<'a> {
    a11y: &'a A11y,
    visited: usize,
    truncated: bool,
    max_depth: u32,
    all: bool,
}

impl Walk<'_> {
    fn tree(
        &mut self,
        r: &ARef,
        offset: Option<(Rect, Rect)>,
        depth: u32,
        printed_depth: usize,
        out: &mut String,
    ) {
        if self.visited >= NODE_BUDGET {
            self.truncated = true;
            return;
        }
        self.visited += 1;

        let node = self.a11y.resolve(r, offset);
        let boring = !self.all && is_boring(&node);
        let next_printed = if boring {
            printed_depth
        } else {
            render(&node, printed_depth, out);
            printed_depth + 1
        };

        if depth >= self.max_depth {
            return;
        }
        if let Ok(children) = self.a11y.children(r) {
            for child in children {
                self.tree(&child, offset, depth + 1, next_printed, out);
            }
        }
    }

    fn find(
        &mut self,
        r: &ARef,
        offset: Option<(Rect, Rect)>,
        query: &str,
        role: Option<&str>,
        depth: u32,
        hits: &mut Vec<Node>,
    ) {
        if self.visited >= NODE_BUDGET || hits.len() >= 40 {
            self.truncated = self.visited >= NODE_BUDGET;
            return;
        }
        self.visited += 1;

        let node = self.a11y.resolve(r, offset);
        let name_hit = query.is_empty() || node.name.to_lowercase().contains(query);
        let role_hit = role.is_none_or(|q| node.role.eq_ignore_ascii_case(q));
        if name_hit && role_hit && !(query.is_empty() && role.is_none() && is_boring(&node)) {
            hits.push(node);
        }

        if depth >= self.max_depth {
            return;
        }
        if let Ok(children) = self.a11y.children(r) {
            for child in children {
                self.find(&child, offset, query, role, depth + 1, hits);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry points used by the executor
// ---------------------------------------------------------------------------

/// Resolve which window to inspect, its app, frame, and coordinate offset.
fn window_context(
    a11y: &A11y,
    comp: &dyn Compositor,
    window: &Option<String>,
) -> Result<(WindowInfo, ARef, Rect)> {
    let win = match window {
        Some(addr) => comp.window_by_address(addr)?,
        None => comp.active_window()?.ok_or_else(|| {
            Error::with_hint(
                "no window is focused",
                "pass `window` with an address from list_windows",
            )
        })?,
    };
    let app = a11y.app_for_window(&win)?;
    let (frame, origin) = a11y.frame_for_window(&app, &win)?;
    Ok((win, frame, origin))
}

pub fn ui_tree(
    a11y: &A11y,
    comp: &dyn Compositor,
    window: &Option<String>,
    depth: Option<u32>,
    all: bool,
) -> Result<String> {
    let (win, frame, origin) = window_context(a11y, comp, window)?;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "accessibility tree of {} ({}) — \"{}\"\n\
         coordinates are absolute layout (x,y = element centre) — usable directly \
         with click/move_cursor. «id» tokens go to element_action / element_read / \
         element_set_text / element_focus.\n",
        win.class, win.address, win.title
    );

    let mut walk = Walk {
        a11y,
        visited: 0,
        truncated: false,
        max_depth: depth.unwrap_or(DEFAULT_DEPTH),
        all,
    };
    walk.tree(&frame, Some((win.geometry, origin)), 0, 0, &mut out);

    if walk.truncated {
        let _ = writeln!(
            out,
            "\n[truncated after {NODE_BUDGET} nodes — narrow with find_element, \
             or lower `depth`]"
        );
    }
    Ok(out)
}

pub fn find_element(
    a11y: &A11y,
    comp: &dyn Compositor,
    query: &str,
    role: Option<&str>,
    window: &Option<String>,
) -> Result<String> {
    let query_lc = query.to_lowercase();
    let mut hits = Vec::new();
    let mut walk = Walk {
        a11y,
        visited: 0,
        truncated: false,
        max_depth: 40,
        all: true,
    };
    let mut searched = Vec::new();

    if window.is_some() {
        let (win, frame, origin) = window_context(a11y, comp, window)?;
        searched.push(format!("{} ({})", win.class, win.address));
        walk.find(
            &frame,
            Some((win.geometry, origin)),
            &query_lc,
            role,
            0,
            &mut hits,
        );
    } else {
        // All accessible apps that map to an open window.
        let windows = comp.windows()?;
        for (app, pid) in a11y.applications()? {
            let Some(win) = windows
                .iter()
                .find(|w| pid.map(|p| p as i64) == Some(w.pid))
            else {
                continue;
            };
            searched.push(format!("{} ({})", win.class, win.address));
            let Ok((frame, origin)) = a11y.frame_for_window(&app, win) else {
                continue;
            };
            walk.find(
                &frame,
                Some((win.geometry, origin)),
                &query_lc,
                role,
                0,
                &mut hits,
            );
        }
        if searched.is_empty() {
            return Err(Error::with_hint(
                "no accessible application maps to any open window",
                "no running app is registered on the accessibility bus — see \
                 the doctor tool's accessibility section for per-toolkit fixes",
            ));
        }
    }

    let mut out = String::new();
    if hits.is_empty() {
        let _ = writeln!(
            out,
            "no elements matching name~{query:?}{} in: {}",
            role.map(|r| format!(" role={r}")).unwrap_or_default(),
            searched.join(", ")
        );
        let _ = writeln!(
            out,
            "try a shorter substring, drop `role`, or read the full ui_tree."
        );
    } else {
        let _ = writeln!(
            out,
            "{} match(es) (centre coordinates are absolute):",
            hits.len()
        );
        for node in &hits {
            render(node, 0, &mut out);
        }
    }
    if walk.truncated {
        let _ = writeln!(
            out,
            "[search truncated at {NODE_BUDGET} nodes — narrow the query]"
        );
    }
    Ok(out)
}

pub fn element_action(a11y: &A11y, element: &str, action: Option<&str>) -> Result<String> {
    let r = A11y::parse_element_id(element)?;
    let names = a11y.action_names(&r);
    if names.is_empty() {
        return Err(Error::with_hint(
            format!("element {element} exposes no actions"),
            "click its coordinates instead, or element_focus it and use key",
        ));
    }
    let index = match action {
        Some(want) => names
            .iter()
            .position(|n| n.eq_ignore_ascii_case(want))
            .ok_or_else(|| {
                Error::new(format!(
                    "element has no action {want:?} — available: {}",
                    names.join(", ")
                ))
            })?,
        None => 0,
    };
    let (ok,): (bool,) = a11y.call(
        &r.0,
        r.1.as_str(),
        IFACE_ACTION,
        "DoAction",
        &(index as i32,),
    )?;
    if ok {
        Ok(format!(
            "invoked {:?} on {element}. Verify the effect (screenshot or ui_tree) \
             before assuming it worked.",
            names[index]
        ))
    } else {
        Err(Error::new(format!(
            "app rejected action {:?} on {element}",
            names[index]
        )))
    }
}

pub fn element_read(a11y: &A11y, element: &str) -> Result<String> {
    let r = A11y::parse_element_id(element)?;
    let node = a11y.resolve(&r, None);

    if node.has_text {
        let count = a11y
            .property_i32(&r, IFACE_TEXT, "CharacterCount")
            .unwrap_or(0);
        let (text,): (String,) =
            a11y.call(&r.0, r.1.as_str(), IFACE_TEXT, "GetText", &(0i32, count))?;
        return Ok(format!(
            "{} \"{}\" — text ({count} chars):\n{text}",
            node.role, node.name
        ));
    }
    if let Some(v) = node.value {
        return Ok(format!("{} \"{}\" — value: {v}", node.role, node.name));
    }
    Ok(format!(
        "{} \"{}\" — no text or value interface. States: [{}]",
        node.role,
        node.name,
        node.states.join(",")
    ))
}

pub fn element_set_text(a11y: &A11y, element: &str, text: &str) -> Result<String> {
    let r = A11y::parse_element_id(element)?;
    let (ok,): (bool,) = a11y
        .call(
            &r.0,
            r.1.as_str(),
            IFACE_EDITABLE,
            "SetTextContents",
            &(text,),
        )
        .map_err(|e| {
            Error::with_hint(
                format!("set_text failed: {e}"),
                "the element may not be editable — check for {set_text} in ui_tree; \
                 fall back to element_focus + type_text",
            )
        })?;
    if ok {
        Ok(format!(
            "replaced text contents of {element} with {} character(s). Verify with \
             element_read — some apps report success without applying.",
            text.chars().count()
        ))
    } else {
        Err(Error::with_hint(
            format!("app refused to set text on {element}"),
            "fall back to element_focus + type_text",
        ))
    }
}

pub fn element_focus(a11y: &A11y, element: &str) -> Result<String> {
    let r = A11y::parse_element_id(element)?;
    let (ok,): (bool,) = a11y.call(&r.0, r.1.as_str(), IFACE_COMPONENT, "GrabFocus", &())?;
    if ok {
        Ok(format!("focus grabbed by {element}"))
    } else {
        Err(Error::new(format!("app refused focus grab on {element}")))
    }
}

/// Current value of the a11y status screen-reader flag, if readable.
fn screen_reader_flag() -> Option<bool> {
    let session = Connection::session().ok()?;
    let msg = session
        .call_method(
            Some("org.a11y.Bus"),
            "/org/a11y/bus",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.a11y.Status", "ScreenReaderEnabled"),
        )
        .ok()?;
    let (v,): (zbus::zvariant::OwnedValue,) = msg.body().deserialize().ok()?;
    bool::try_from(v).ok()
}

/// One-line-per-app coverage summary for doctor.
pub fn doctor_summary(comp: &dyn Compositor) -> String {
    let a11y = match A11y::connect() {
        Ok(a) => a,
        Err(e) => {
            let mut s = format!("  [--]   accessibility bus unavailable: {}\n", e.message);
            if std::env::var("NO_AT_BRIDGE").is_ok_and(|v| v == "1") {
                s.push_str("  [--]   NO_AT_BRIDGE=1 is set — apps will not register even if the bus comes up\n");
            }
            s.push_str("         semantic tools (ui_tree, find_element, element_*) are unavailable; screenshot flow still works\n");
            return s;
        }
    };

    let apps = match a11y.applications() {
        Ok(a) => a,
        Err(e) => return format!("  [FAIL] a11y bus up but registry unreadable: {e}\n"),
    };

    let windows = comp.windows().unwrap_or_default();
    let covered: Vec<&WindowInfo> = windows
        .iter()
        .filter(|w| {
            apps.iter()
                .any(|(_, pid)| pid.map(|p| p as i64) == Some(w.pid))
        })
        .collect();

    let mut s = format!(
        "  [ok]   bus up, {} app(s) registered; {}/{} open windows are accessible\n",
        apps.len(),
        covered.len(),
        windows.len()
    );
    match screen_reader_flag() {
        Some(true) => s.push_str(
            "  [ok]   screen-reader flag advertised — Electron/Chromium/Firefox enable their trees\n",
        ),
        _ => s.push_str(
            "  [--]   screen-reader flag off — Electron/Chromium stay inaccessible until a \
             launch turns it on\n",
        ),
    }
    for w in &windows {
        let ok = covered.iter().any(|c| c.address == w.address);
        let mark = if ok { "[ok]  " } else { "[--]  " };
        let _ = writeln!(s, "  {mark} {} ({})", w.class, w.address);
    }
    if covered.len() < windows.len() {
        s.push_str(
            "         missing apps: Electron needs --force-renderer-accessibility, \
             Qt needs QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1, and registration only \
             happens at app startup\n",
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn element_ids_round_trip() {
        // Flat GTK-style path.
        let r = A11y::parse_element_id(":1.42|216").unwrap();
        assert_eq!(r.0, ":1.42");
        assert_eq!(r.1.as_str(), "/org/a11y/atspi/accessible/216");
        assert_eq!(A11y::element_id(&r), ":1.42|216");

        // Nested AccessKit-style path survives the trip too.
        let r = A11y::parse_element_id(":1.39|0/88804201510221976").unwrap();
        assert_eq!(
            r.1.as_str(),
            "/org/a11y/atspi/accessible/0/88804201510221976"
        );
        assert_eq!(A11y::element_id(&r), ":1.39|0/88804201510221976");

        // App-custom absolute paths (GTK4 apps use their own prefix).
        let r = A11y::parse_element_id(":1.0|/org/gnome/Nautilus/a11y/abc").unwrap();
        assert_eq!(r.1.as_str(), "/org/gnome/Nautilus/a11y/abc");
        assert_eq!(A11y::element_id(&r), ":1.0|/org/gnome/Nautilus/a11y/abc");
    }

    #[test]
    fn malformed_element_ids_error() {
        assert!(A11y::parse_element_id("no-separator").is_err());
        assert!(A11y::parse_element_id(":1.2|with spaces").is_err());
    }

    #[test]
    fn role_table_matches_atspi_enum() {
        // Spot-check well-known values against the AT-SPI constants.
        assert_eq!(ROLE_NAMES[23], "frame");
        assert_eq!(ROLE_NAMES[43], "push button");
        assert_eq!(ROLE_NAMES[61], "text");
        assert_eq!(ROLE_NAMES[79], "entry");
        assert_eq!(ROLE_NAMES[95], "document web");
    }
}
