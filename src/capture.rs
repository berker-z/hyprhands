//! Screen capture via grim.
//!
//! Two things here are load-bearing, both learned the hard way:
//!
//! 1. `grim -g` captures a *screen region*, not a window. If the target window
//!    isn't currently rendered — because its workspace isn't displayed on any
//!    monitor — you silently get whatever else is painted at those
//!    coordinates. We refuse instead. See [`ensure_visible`].
//! 2. Full-resolution captures are expensive (a 2554x1399 window is ~4.8k image
//!    tokens). `grim -s` downscales during capture, and we report the factor so
//!    coordinates still map back exactly. Downscaling costs no targeting
//!    accuracy as long as the caller does the arithmetic, so the note spells it
//!    out rather than assuming.

use crate::action::{CaptureTarget, Error, Rect, Result};
use crate::compositor::{Compositor, WindowInfo};
use crate::sh;

/// Long-edge pixel budget before current models downscale server-side.
/// Captures at or below this map 1:1 to the coordinates the model emits.
pub const NATIVE_COORD_LIMIT: i32 = 2576;

/// Default long-edge cap. Above this we downscale automatically — roughly a 4x
/// token saving at 0.5, with no loss of targeting accuracy.
pub const DEFAULT_MAX_LONG_EDGE: i32 = 1400;

pub struct Capture {
    pub png: Vec<u8>,
    pub note: String,
}

pub fn capture(
    comp: &dyn Compositor,
    target: &CaptureTarget,
    scale: Option<f64>,
) -> Result<Capture> {
    let (args, description, region) = match target {
        // Compute the layout bounding box so a full-screen grab still gets
        // auto-downscaled and still reports a coordinate origin. Without this
        // the single most expensive capture is the one that escapes the cap.
        CaptureTarget::All => (
            vec![],
            "full screen".to_string(),
            layout_bounds(&comp.monitors()?),
        ),

        CaptureTarget::Monitor(name) => {
            let monitors = comp.monitors()?;
            let monitor = monitors
                .iter()
                .find(|m| &m.name == name)
                .ok_or_else(|| {
                    let names: Vec<&str> = monitors.iter().map(|m| m.name.as_str()).collect();
                    Error::with_hint(
                        format!("no monitor named {name:?}"),
                        format!("available: {}", names.join(", ")),
                    )
                })?
                .clone();
            (
                vec!["-o".to_string(), name.clone()],
                format!("monitor {name}"),
                Some(monitor.geometry),
            )
        }

        CaptureTarget::Window(address) => {
            let window = comp.window_by_address(address)?;
            ensure_visible(comp, &window)?;
            (
                vec!["-g".to_string(), window.geometry.grim_geometry()],
                format!("window {} ({})", window.address, window.class),
                Some(window.geometry),
            )
        }

        CaptureTarget::ActiveWindow => {
            let window = comp.active_window()?.ok_or_else(|| {
                Error::with_hint(
                    "no window is currently focused",
                    "use list_windows and pass an explicit address, or capture the full screen",
                )
            })?;
            ensure_visible(comp, &window)?;
            (
                vec!["-g".to_string(), window.geometry.grim_geometry()],
                format!("active window {} ({})", window.address, window.class),
                Some(window.geometry),
            )
        }

        CaptureTarget::Region(rect) => (
            vec!["-g".to_string(), rect.grim_geometry()],
            format!("region {}x{} at ({}, {})", rect.w, rect.h, rect.x, rect.y),
            Some(*rect),
        ),
    };

    let effective_scale = resolve_scale(scale, region.as_ref());

    let mut argv: Vec<String> = args;
    if let Some(s) = effective_scale {
        argv.push("-s".to_string());
        argv.push(format!("{s}"));
    }
    argv.extend(["-t".to_string(), "png".to_string(), "-".to_string()]);

    let refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    let png = sh::run_bytes("grim", &refs)?;
    if png.is_empty() {
        return Err(Error::new("grim produced an empty image"));
    }

    Ok(Capture {
        note: build_note(&description, region.as_ref(), effective_scale, png.len()),
        png,
    })
}

/// Refuse to capture a window that isn't being rendered.
///
/// A window is visible iff its workspace is the active one on some monitor.
/// Hyprland's `mapped`/`hidden` fields are NOT a substitute — an off-workspace
/// window reports `mapped: true, hidden: false` while rendering nowhere, which
/// is exactly how this bug hid.
fn ensure_visible(comp: &dyn Compositor, window: &WindowInfo) -> Result<()> {
    let monitors = comp.monitors()?;
    let visible = monitors
        .iter()
        .any(|m| m.active_workspace == window.workspace);

    if visible {
        return Ok(());
    }

    let showing: Vec<String> = monitors
        .iter()
        .map(|m| format!("{} shows workspace {}", m.name, m.active_workspace))
        .collect();

    Err(Error::with_hint(
        format!(
            "{} ({}) is on workspace {}, which is not displayed on any monitor — \
             capturing its geometry would return unrelated pixels",
            window.class, window.address, window.workspace
        ),
        format!(
            "{}. Bring it on screen first via focus_window(\"{}\"), \
             or capture a window that is currently on screen.",
            showing.join("; "),
            window.address
        ),
    ))
}

/// Below this, downscaling isn't worth the coordinate-conversion overhead —
/// shaving 1% off a capture just makes the arithmetic uglier for no saving.
const SCALE_DEADBAND: f64 = 0.9;

/// Union of all monitor rectangles — the coordinate space a full-screen grab
/// covers.
fn layout_bounds(monitors: &[crate::compositor::MonitorInfo]) -> Option<Rect> {
    let first = monitors.first()?;
    let (mut x0, mut y0) = (first.geometry.x, first.geometry.y);
    let (mut x1, mut y1) = (
        first.geometry.x + first.geometry.w,
        first.geometry.y + first.geometry.h,
    );
    for m in monitors.iter().skip(1) {
        x0 = x0.min(m.geometry.x);
        y0 = y0.min(m.geometry.y);
        x1 = x1.max(m.geometry.x + m.geometry.w);
        y1 = y1.max(m.geometry.y + m.geometry.h);
    }
    Some(Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

/// Pick a scale factor: an explicit request wins, else auto-downscale oversized
/// captures, else native.
fn resolve_scale(requested: Option<f64>, region: Option<&Rect>) -> Option<f64> {
    if let Some(s) = requested {
        let clamped = s.clamp(0.1, 1.0);
        return if clamped >= 1.0 { None } else { Some(clamped) };
    }
    let long_edge = region.map(|r| r.w.max(r.h))?;
    if long_edge <= DEFAULT_MAX_LONG_EDGE {
        return None;
    }
    // Round down to 2dp so the reported factor is clean to invert by hand.
    let factor = ((DEFAULT_MAX_LONG_EDGE as f64 / long_edge as f64) * 100.0).floor() / 100.0;
    if factor > SCALE_DEADBAND {
        None
    } else {
        Some(factor)
    }
}

fn build_note(
    description: &str,
    region: Option<&Rect>,
    scale: Option<f64>,
    bytes: usize,
) -> String {
    let mut note = format!("Captured {description} ({} KiB PNG).", bytes / 1024);

    let Some(rect) = region else {
        if let Some(s) = scale {
            note.push_str(&format!(
                " Downscaled by {s}; divide any coordinate read off this image \
                 by {s} to get desktop coordinates."
            ));
        }
        return note;
    };

    match scale {
        // Native: image pixels are desktop pixels, only the offset applies.
        None => note.push_str(&format!(
            " Image is {}x{}; its top-left is at absolute ({}, {}). Add that \
             offset to any coordinate you read off this image before passing it \
             to click or move_cursor.",
            rect.w, rect.h, rect.x, rect.y
        )),
        // Scaled: divide by the factor, *then* add the offset.
        Some(s) => note.push_str(&format!(
            " Downscaled by {s} — it covers a {}x{} region whose top-left is at \
             absolute ({}, {}). To convert a point (ix, iy) from this image into \
             desktop coordinates: x = {} + ix / {s}, y = {} + iy / {s}. Request \
             scale=1.0 if you need to read fine detail.",
            rect.w, rect.h, rect.x, rect.y, rect.x, rect.y
        )),
    }

    let long_edge = rect.w.max(rect.h);
    if scale.is_none() && long_edge > NATIVE_COORD_LIMIT {
        note.push_str(&format!(
            " NOTE: long edge is {long_edge}px, above the {NATIVE_COORD_LIMIT}px \
             threshold, so it may be downscaled in transit and coordinates read \
             from it may be proportionally off."
        ));
    }

    note
}
