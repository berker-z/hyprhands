//! The executor: `Action` in, `Observation` out.
//!
//! This is the seam. Any adapter — MCP today, a native computer-use loop
//! later — should only ever construct `Action`s and call `execute`.

use crate::action::{Action, CaptureTarget, Error, Observation, Result};
use crate::capture;
use crate::compositor::{self, Compositor, WindowInfo};
use crate::input;
use serde::Serialize;
use std::time::Duration;

/// How long to wait for a requested focus change to land before refusing.
const FOCUS_POLL_ATTEMPTS: u32 = 20;
const FOCUS_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Human-readable window label for observations and errors.
fn describe(w: &WindowInfo) -> String {
    if w.title.is_empty() {
        format!("{} ({})", w.class, w.address)
    } else {
        format!("{} ({}) — \"{}\"", w.class, w.address, w.title)
    }
}

pub struct Executor {
    comp: Box<dyn Compositor>,
}

#[derive(Serialize)]
struct DesktopState<'a> {
    compositor: &'a str,
    cursor: CursorState,
    active_window: Option<&'a crate::compositor::WindowInfo>,
    monitors: Vec<crate::compositor::MonitorInfo>,
    windows: Vec<crate::compositor::WindowInfo>,
}

#[derive(Serialize)]
struct CursorState {
    x: i32,
    y: i32,
}

impl Executor {
    pub fn new() -> Result<Self> {
        Ok(Executor {
            comp: compositor::detect()?,
        })
    }

    pub fn compositor(&self) -> &dyn Compositor {
        self.comp.as_ref()
    }

    pub fn execute(&self, action: &Action) -> Result<Observation> {
        match action {
            Action::Screenshot { target, scale } => self.screenshot(target, *scale),
            Action::Doctor => Ok(Observation::text(crate::doctor::report())),
            Action::ListWindows => self.list_windows(),
            Action::CursorPosition => {
                let p = self.comp.cursor_position()?;
                Ok(Observation::text(format!(
                    "cursor is at ({}, {})",
                    p.x, p.y
                )))
            }
            Action::MoveCursor(point) => {
                input::move_cursor(self.comp.as_ref(), *point)?;
                Ok(Observation::text(format!(
                    "moved cursor to ({}, {})",
                    point.x, point.y
                )))
            }
            Action::Click { at, button, window } => {
                let focused = self.ensure_focus(window)?;
                input::click(self.comp.as_ref(), *at, *button)?;
                let where_ = match at {
                    Some(p) => format!("at ({}, {})", p.x, p.y),
                    None => {
                        let p = self.comp.cursor_position()?;
                        format!("at current cursor position ({}, {})", p.x, p.y)
                    }
                };
                Ok(Observation::text(format!(
                    "{:?} click {where_} — delivered to {}",
                    button,
                    describe(&focused)
                )))
            }
            Action::TypeText { text, window } => {
                let focused = self.ensure_focus(window)?;
                input::type_text(text)?;
                Ok(Observation::text(format!(
                    "typed {} character(s) — delivered to {}",
                    text.chars().count(),
                    describe(&focused)
                )))
            }
            Action::Key { chord, window } => {
                let focused = self.ensure_focus(window)?;
                input::key(self.comp.as_ref(), chord)?;
                Ok(Observation::text(format!(
                    "sent key chord {chord} — delivered to {}",
                    describe(&focused)
                )))
            }
            Action::Scroll {
                direction,
                amount,
                window,
            } => {
                let focused = self.ensure_focus(window)?;
                input::scroll(*direction, *amount)?;
                Ok(Observation::text(format!(
                    "scrolled {direction:?} by {amount} — delivered to {}",
                    describe(&focused)
                )))
            }
            Action::FocusWindow(address) => {
                let window = self.comp.window_by_address(address)?;
                self.comp.focus_window(address)?;
                Ok(Observation::text(format!(
                    "focused {} ({}) — \"{}\"",
                    window.address, window.class, window.title
                )))
            }
            Action::Launch(command) => {
                self.comp.launch(command)?;
                Ok(Observation::text(format!(
                    "launched: {command}\n\nThe window may take a moment to map. \
                     Call list_windows to confirm before interacting with it."
                )))
            }
        }
    }

    /// Resolve the window that input is about to be delivered to.
    ///
    /// With `window` set: focus it, then poll until the compositor agrees it is
    /// focused. If it never lands we refuse rather than deliver keystrokes
    /// somewhere unintended — that failure is silent and can be destructive.
    ///
    /// With `window` unset: report whatever currently has focus, so a
    /// wrong-window delivery is at least visible in the observation.
    fn ensure_focus(&self, window: &Option<String>) -> Result<WindowInfo> {
        let Some(address) = window else {
            return self.comp.active_window()?.ok_or_else(|| {
                Error::with_hint(
                    "no window is focused — input would go nowhere",
                    "pass the `window` argument with an address from list_windows",
                )
            });
        };

        let target = self.comp.window_by_address(address)?;
        self.comp.focus_window(address)?;

        // Focus changes are applied asynchronously by the compositor.
        for _ in 0..FOCUS_POLL_ATTEMPTS {
            if let Some(active) = self.comp.active_window()? {
                if &active.address == address {
                    return Ok(active);
                }
            }
            std::thread::sleep(FOCUS_POLL_INTERVAL);
        }

        let actual = self
            .comp
            .active_window()?
            .map(|w| describe(&w))
            .unwrap_or_else(|| "nothing".to_string());

        Err(Error::with_hint(
            format!(
                "asked to focus {} but {actual} still has focus after {}ms — refusing to \
                 send input, since it would go to the wrong window",
                describe(&target),
                FOCUS_POLL_ATTEMPTS * FOCUS_POLL_INTERVAL.as_millis() as u32
            ),
            "the window may be on another workspace, or something is holding focus \
             (a modal, a permission prompt). Check list_windows and try focus_window \
             on its own first.",
        ))
    }

    fn screenshot(&self, target: &CaptureTarget, scale: Option<f64>) -> Result<Observation> {
        let shot = capture::capture(self.comp.as_ref(), target, scale)?;
        Ok(Observation::Image {
            png: shot.png,
            note: shot.note,
        })
    }

    fn list_windows(&self) -> Result<Observation> {
        let windows = self.comp.windows()?;
        let active = self.comp.active_window()?;
        let cursor = self.comp.cursor_position()?;
        let monitors = self.comp.monitors()?;

        let state = DesktopState {
            compositor: self.comp.name(),
            cursor: CursorState {
                x: cursor.x,
                y: cursor.y,
            },
            active_window: active.as_ref(),
            monitors,
            windows,
        };

        serde_json::to_string_pretty(&state)
            .map(Observation::Text)
            .map_err(|e| crate::action::Error::new(format!("could not serialise state: {e}")))
    }
}
