//! The executor: `Action` in, `Observation` out.
//!
//! Adapters stop here. Any of them — MCP today, a native computer-use loop
//! later — should only ever construct `Action`s and call `execute`.

use crate::a11y::{self, A11y};
use crate::action::{Action, CaptureTarget, Error, Observation, Result};
use crate::capture;
use crate::compositor::{self, Compositor, WindowInfo};
use crate::input;
use crate::notes;
use serde::Serialize;
use std::sync::OnceLock;
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
    /// Lazily connected: only sessions that use semantic tools pay for it, and
    /// a failed connect is retried on the next call (the bus can come up later).
    a11y: OnceLock<A11y>,
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
            a11y: OnceLock::new(),
        })
    }

    fn a11y(&self) -> Result<&A11y> {
        if let Some(a) = self.a11y.get() {
            return Ok(a);
        }
        let conn = A11y::connect()?;
        Ok(self.a11y.get_or_init(|| conn))
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
            Action::Drag {
                from,
                to,
                button,
                window,
            } => {
                let focused = self.ensure_focus(window)?;
                input::drag(self.comp.as_ref(), *from, *to, *button)?;
                Ok(Observation::text(format!(
                    "{:?}-dragged from ({}, {}) to ({}, {}) — delivered to {}",
                    button,
                    from.x,
                    from.y,
                    to.x,
                    to.y,
                    describe(&focused)
                )))
            }
            Action::MoveWindow { address, to } => {
                self.comp.window_by_address(address)?;
                self.comp.move_window(address, *to)?;
                let after = self.comp.window_by_address(address)?;
                let g = after.geometry;
                if (g.x, g.y) == (to.x, to.y) {
                    Ok(Observation::text(format!(
                        "moved {} to ({}, {})",
                        describe(&after),
                        g.x,
                        g.y
                    )))
                } else {
                    // Hyprland answers "ok" and does nothing for tiled windows,
                    // so requested-vs-actual is the only honest report.
                    Ok(Observation::text(format!(
                        "asked to move {} to ({}, {}) but it is at ({}, {}){}",
                        describe(&after),
                        to.x,
                        to.y,
                        g.x,
                        g.y,
                        if after.floating {
                            ""
                        } else {
                            " — it is tiled, and the compositor only moves floating \
                             windows freely. Float it first if you need exact placement."
                        }
                    )))
                }
            }
            Action::ResizeWindow { address, w, h } => {
                self.comp.window_by_address(address)?;
                self.comp.resize_window(address, *w, *h)?;
                let after = self.comp.window_by_address(address)?;
                let g = after.geometry;
                if (g.w, g.h) == (*w, *h) {
                    Ok(Observation::text(format!(
                        "resized {} to {}x{}",
                        describe(&after),
                        g.w,
                        g.h
                    )))
                } else {
                    Ok(Observation::text(format!(
                        "asked to resize {} to {}x{} but it is {}x{}{}",
                        describe(&after),
                        w,
                        h,
                        g.w,
                        g.h,
                        if after.floating {
                            ""
                        } else {
                            " — it is tiled, so the compositor adjusted the layout \
                             split as far as the layout allows. Float it first if \
                             you need an exact size."
                        }
                    )))
                }
            }
            Action::FocusWindow(address) => {
                let window = self.comp.window_by_address(address)?;
                self.comp.focus_window(address)?;
                Ok(Observation::text(format!(
                    "focused {} ({}) — \"{}\"",
                    window.address, window.class, window.title
                )))
            }
            Action::UiTree { window, depth, all } => {
                a11y::ui_tree(self.a11y()?, self.comp.as_ref(), window, *depth, *all)
                    .map(Observation::Text)
            }
            Action::FindElement {
                query,
                role,
                window,
            } => a11y::find_element(
                self.a11y()?,
                self.comp.as_ref(),
                query,
                role.as_deref(),
                window,
            )
            .map(Observation::Text),
            Action::ElementAction { element, action } => {
                a11y::element_action(self.a11y()?, element, action.as_deref())
                    .map(Observation::Text)
            }
            Action::ElementRead { element } => {
                a11y::element_read(self.a11y()?, element).map(Observation::Text)
            }
            Action::ElementSetText { element, text } => {
                a11y::element_set_text(self.a11y()?, element, text).map(Observation::Text)
            }
            Action::ElementFocus { element } => {
                a11y::element_focus(self.a11y()?, element).map(Observation::Text)
            }
            Action::AppNotes { app } => notes::read(self.comp.as_ref(), app).map(Observation::Text),
            Action::AppNotesWrite {
                app,
                content,
                version,
            } => notes::write(self.comp.as_ref(), app, content, version.as_deref())
                .map(Observation::Text),
            Action::Launch {
                command,
                wait_seconds,
            } => {
                let (wrapped, accessible) = a11y::accessible_command(command);
                let wait = Duration::from_secs(u64::from(wait_seconds.unwrap_or(8)).min(30));
                let started = std::time::Instant::now();
                let opened = self.comp.launch_awaited(&wrapped, wait)?;
                let a11y_note = if accessible {
                    "\nAccessibility was enabled for it (Qt env + screen-reader \
                     flag), so ui_tree may work on it."
                } else {
                    ""
                };

                let outcome = if opened.is_empty() {
                    if wait.is_zero() {
                        "Not waiting for a window. Call list_windows before \
                         interacting with it."
                            .to_string()
                    } else {
                        format!(
                            "No window mapped within {:.1}s. The app may be slow, \
                             windowless, or a single-instance app that deferred to \
                             an existing process — check list_windows.",
                            wait.as_secs_f32()
                        )
                    }
                } else {
                    let lines: Vec<String> = opened
                        .iter()
                        .map(|w| {
                            format!(
                                "  {} ({}) — \"{}\" on workspace {}",
                                w.address, w.class, w.title, w.workspace
                            )
                        })
                        .collect();
                    format!(
                        "Mapped after {:.1}s:\n{}\nUse the address directly — no \
                         need to call list_windows first.",
                        started.elapsed().as_secs_f32(),
                        lines.join("\n")
                    )
                };

                let mut apps: Vec<String> = opened.iter().map(|w| w.class.clone()).collect();
                apps.sort_unstable();
                apps.dedup();

                Ok(Observation::text_for_apps(
                    format!("launched: {command}\n\n{outcome}{a11y_note}"),
                    apps,
                ))
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
            if let Some(active) = self.comp.active_window()?
                && &active.address == address
            {
                return Ok(active);
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
