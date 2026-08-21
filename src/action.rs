//! Provider-neutral core types.
//!
//! Nothing in this module knows about MCP, Anthropic, OpenAI, or Hyprland.
//! Adapters (see `mcp.rs`) translate inbound schemas into these types; backends
//! (`compositor.rs`, `input.rs`, `capture.rs`) translate them into syscalls.
//! Keep it that way — it is the only reason this stays portable.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "left" | "l" => Ok(Button::Left),
            "right" | "r" => Ok(Button::Right),
            "middle" | "m" => Ok(Button::Middle),
            other => Err(Error::new(format!(
                "unknown mouse button {other:?} (expected left, right, or middle)"
            ))),
        }
    }

    /// `wlrctl pointer click <name>`
    pub fn wlrctl_name(self) -> &'static str {
        match self {
            Button::Left => "left",
            Button::Right => "right",
            Button::Middle => "middle",
        }
    }

    /// ydotool click codes: 0xC0 = left, 0xC1 = right, 0xC2 = middle.
    pub fn ydotool_code(self) -> &'static str {
        match self {
            Button::Left => "0xC0",
            Button::Right => "0xC1",
            Button::Middle => "0xC2",
        }
    }

    /// ydotool press-only code (0x40 | button), for drag.
    pub fn ydotool_down_code(self) -> &'static str {
        match self {
            Button::Left => "0x40",
            Button::Right => "0x41",
            Button::Middle => "0x42",
        }
    }

    /// ydotool release-only code (0x80 | button), for drag.
    pub fn ydotool_up_code(self) -> &'static str {
        match self {
            Button::Left => "0x80",
            Button::Right => "0x81",
            Button::Middle => "0x82",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// Logical-layout rectangle, as reported by the compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    /// grim's `-g` geometry syntax.
    pub fn grim_geometry(&self) -> String {
        format!("{},{} {}x{}", self.x, self.y, self.w, self.h)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

impl ScrollDirection {
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "up" => Ok(ScrollDirection::Up),
            "down" => Ok(ScrollDirection::Down),
            "left" => Ok(ScrollDirection::Left),
            "right" => Ok(ScrollDirection::Right),
            other => Err(Error::new(format!(
                "unknown scroll direction {other:?} (expected up, down, left, or right)"
            ))),
        }
    }

    /// (dy, dx) deltas for `wlrctl pointer scroll <dy> <dx>`.
    pub fn deltas(self, amount: i32) -> (i32, i32) {
        match self {
            ScrollDirection::Up => (-amount, 0),
            ScrollDirection::Down => (amount, 0),
            ScrollDirection::Left => (0, -amount),
            ScrollDirection::Right => (0, amount),
        }
    }
}

/// What a screenshot should cover.
#[derive(Debug, Clone)]
pub enum CaptureTarget {
    /// Everything, across all outputs.
    All,
    /// A single output by connector name, e.g. `HDMI-A-1`.
    Monitor(String),
    /// A window, addressed by compositor handle. Cropped to its geometry.
    Window(String),
    /// The currently focused window.
    ActiveWindow,
    /// An explicit region in logical coordinates.
    Region(Rect),
}

/// Every input-bearing variant carries an optional `window`.
///
/// Input goes to whatever holds focus *at the moment it is delivered*, which is
/// not necessarily what the caller last observed — a permission prompt, a
/// notification, or the user alt-tabbing is enough to move it. Supplying
/// `window` makes the executor focus and *verify* before acting, turning a
/// silent wrong-window delivery into a refusal.
// `ElementAction` deliberately echoes the enum name — it mirrors the MCP tool.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone)]
pub enum Action {
    Screenshot {
        target: CaptureTarget,
        /// `None` auto-downscales oversized captures; `Some(1.0)` forces native.
        scale: Option<f64>,
    },
    Doctor,
    ListWindows,
    CursorPosition,
    MoveCursor(Point),
    Click {
        /// `None` clicks wherever the cursor already is.
        at: Option<Point>,
        button: Button,
        /// Focus this window first, and refuse to act if focus doesn't land.
        /// See [`Action`] docs on why omitting this is risky.
        window: Option<String>,
    },
    TypeText {
        text: String,
        window: Option<String>,
    },
    /// A chord such as `ctrl+shift+t`.
    Key {
        chord: String,
        window: Option<String>,
    },
    Scroll {
        direction: ScrollDirection,
        amount: i32,
        window: Option<String>,
    },
    /// Press a button at `from`, move to `to`, release. Needs ydotool: the
    /// compositor and wlrctl expose click but not separate press/release.
    Drag {
        from: Point,
        to: Point,
        button: Button,
        window: Option<String>,
    },
    FocusWindow(String),
    /// Place a window's top-left corner at an absolute position. Tiled windows
    /// ignore this; the observation reports where the window actually ended up.
    MoveWindow {
        address: String,
        to: Point,
    },
    /// Resize a window. On tiled windows the compositor adjusts the layout
    /// split as far as it can; the observation reports the resulting size.
    ResizeWindow {
        address: String,
        w: i32,
        h: i32,
    },
    Launch {
        command: String,
        /// How long to await a mapped window on the compositor's event stream.
        /// `Some(0)` returns immediately; `None` uses the default.
        wait_seconds: Option<u32>,
    },

    // -- semantic (AT-SPI) verbs -------------------------------------------
    /// Dump the accessibility tree of a window.
    UiTree {
        window: Option<String>,
        depth: Option<u32>,
        /// Include unnamed structural containers normally elided.
        all: bool,
    },
    /// Search accessible elements by name substring and/or role.
    FindElement {
        query: String,
        role: Option<String>,
        window: Option<String>,
    },
    /// Invoke an element's own action ("click", "press", ...) — no pointer needed.
    ElementAction {
        element: String,
        action: Option<String>,
    },
    /// Read an element's text or value.
    ElementRead {
        element: String,
    },
    /// Replace an editable element's text contents.
    ElementSetText {
        element: String,
        text: String,
    },
    /// Give an element keyboard focus.
    ElementFocus {
        element: String,
    },

    // -- per-app memory ----------------------------------------------------
    /// Read saved notes about an app (or list all apps with notes).
    AppNotes {
        app: Option<String>,
    },
    /// Save notes about an app; the server stamps binary identity.
    AppNotesWrite {
        app: String,
        content: String,
        version: Option<String>,
    },
}

/// The result of executing an [`Action`].
///
/// Deliberately not an MCP type — adapters encode this into whatever content
/// blocks their protocol wants.
#[derive(Debug)]
pub enum Observation {
    Text(String),
    /// Text tied to application classes discovered by the executor itself.
    /// Adapters can use these authoritative hints for cross-cutting behavior
    /// such as memory without scraping human-readable output.
    TextForApps {
        text: String,
        apps: Vec<String>,
    },
    Image {
        png: Vec<u8>,
        note: String,
    },
}

impl Observation {
    pub fn text(s: impl Into<String>) -> Self {
        Observation::Text(s.into())
    }

    pub fn text_for_apps(s: impl Into<String>, apps: Vec<String>) -> Self {
        Observation::TextForApps {
            text: s.into(),
            apps,
        }
    }
}

#[derive(Debug)]
pub struct Error {
    pub message: String,
    /// Actionable remediation, surfaced to the model so it can adapt rather
    /// than retry the same failing call.
    pub hint: Option<String>,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Error {
            message: message.into(),
            hint: None,
        }
    }

    pub fn with_hint(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Error {
            message: message.into(),
            hint: Some(hint.into()),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(hint) = &self.hint {
            write!(f, "\n\nhint: {hint}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
