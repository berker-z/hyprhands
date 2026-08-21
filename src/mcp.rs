//! MCP adapter: newline-delimited JSON-RPC 2.0 over stdio.
//!
//! Hand-rolled rather than pulled from an SDK — the surface we need is small
//! and fully specified, and this keeps the dependency tree to four crates
//! (serde, serde_json, base64, zbus). Swapping in `rmcp` later touches only
//! this file.
//!
//! stdout is the protocol channel. Anything diagnostic MUST go to stderr or it
//! corrupts the stream.

use crate::action::{
    Action, Button, CaptureTarget, Error, Observation, Point, Rect, Result, ScrollDirection,
};
use crate::exec::Executor;
use crate::notes;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{BufRead, Write};

const SERVER_NAME: &str = "hyprhands";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const FALLBACK_PROTOCOL: &str = "2025-06-18";

const MEMORY_INSTRUCTIONS: &str = "hyprhands manages versioned per-application memory. Saved \
    notes are injected automatically the first time an application is encountered in this \
    server session; do not call app_notes proactively and do not rediscover facts already \
    marked CURRENT. Use app_notes only to list memory or deliberately revisit notes after they \
    have fallen out of context. During GUI work, collect \
    only reusable, verified UI facts such as control names, menu paths, successful workflows, \
    and stable quirks. A claim is verified only when supported by a returned tool result or by \
    notes marked CURRENT; exclude plausible background knowledge that was not exercised. \
    Do not save transient machine/session state such as dependency availability, bus failures, \
    open documents, current media, geometry, focus, or capability errors unless verified as an \
    app-specific stable quirk. Generalize placeholders and omit task-specific or acceptance-test \
    values. After the \
    workflow is verified and before completing the user's task, \
    call app_notes_write with the full replacement notes if new reusable knowledge was learned \
    or stale notes were corrected. Do not record user-entered content, secrets, document data, \
    or guesses. A memory checkpoint in a tool result is an instruction to perform this review, \
    not a request to interrupt an unfinished workflow.";

const WINDOW_ARG: &str = "Address of the window to receive this input, from list_windows. \
     The server focuses it and confirms focus landed before acting, and errors out if it \
     didn't. Omitting this delivers to whatever is focused at that instant, which is a \
     silent-misdelivery risk — always pass it when you know the target.";

pub fn serve() -> i32 {
    // Construct up front so a misconfigured environment fails loudly at
    // startup rather than on the first tool call.
    let executor = match Executor::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("hyprhands: {e}");
            return 1;
        }
    };

    eprintln!(
        "hyprhands {SERVER_VERSION} ready on {} (stdio)",
        executor.compositor().name()
    );

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut memory = MemorySession::default();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("hyprhands: stdin read error: {e}");
                return 1;
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("hyprhands: malformed JSON: {e}");
                continue;
            }
        };

        // Notifications carry no id and must not be answered.
        let Some(id) = request.get("id").cloned() else {
            continue;
        };

        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(json!({}));

        let response = match method {
            "initialize" => success(id, initialize(&params)),
            "ping" => success(id, json!({})),
            "tools/list" => success(id, json!({ "tools": tool_definitions() })),
            "tools/call" => success(id, call_tool(&executor, &mut memory, &params)),
            other => failure(id, -32601, &format!("method not found: {other}")),
        };

        if writeln!(stdout, "{response}").is_err() || stdout.flush().is_err() {
            eprintln!("hyprhands: stdout closed, exiting");
            return 1;
        }
    }

    0
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn failure(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize(params: &Value) -> Value {
    // Echo the client's protocol version when it sends one; clients are
    // stricter about mismatch than about an unfamiliar-but-valid value.
    let protocol = params
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or(FALLBACK_PROTOCOL);

    json!({
        "protocolVersion": protocol,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
        "instructions": MEMORY_INSTRUCTIONS
    })
}

// ---------------------------------------------------------------------------
// Tool surface
// ---------------------------------------------------------------------------

fn tool_definitions() -> Value {
    json!([
        {
            "name": "screenshot",
            "description":
                "Capture the screen as a PNG. Strongly prefer target='active_window' or \
                 target='window' over 'all' — cropping to one window costs far fewer \
                 tokens and makes clicking more accurate. Coordinates you read off a \
                 cropped image are image-local; the returned note gives the absolute \
                 offset you must add before calling click or move_cursor.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "enum": ["all", "active_window", "window", "monitor", "region"],
                        "description": "What to capture. Defaults to active_window."
                    },
                    "address": { "type": "string", "description": "Window handle, when target='window'." },
                    "monitor": { "type": "string", "description": "Connector name, when target='monitor'." },
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                    "width": { "type": "integer" },
                    "height": { "type": "integer" },
                    "scale": {
                        "type": "number",
                        "description":
                            "0.1–1.0. Omit to auto-downscale anything over 1400px on the \
                             long edge — that is ~4x cheaper in tokens and loses no \
                             targeting accuracy, because the returned note tells you \
                             exactly how to convert image coordinates back to desktop \
                             ones. Pass 1.0 only when you need to read fine detail."
                    }
                }
            },
            "annotations": {
                "title": "Screenshot",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "doctor",
            "description":
                "Report which capabilities are available and what is missing. Call this \
                 when a tool fails with a missing-binary error, before giving up — it \
                 names the exact dependency and what still works without it.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": {
                "title": "Diagnostics",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "list_windows",
            "description":
                "List open windows with their absolute geometry, plus monitors, cursor \
                 position, and which window is focused. Call this before interacting \
                 with anything — it is far cheaper than a screenshot and often enough \
                 on its own.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": {
                "title": "Observe windows",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "cursor_position",
            "description": "Get the current absolute pointer position.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": {
                "title": "Cursor position",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "move_cursor",
            "description": "Move the pointer to an absolute position.",
            "inputSchema": {
                "type": "object",
                "properties": { "x": { "type": "integer" }, "y": { "type": "integer" } },
                "required": ["x", "y"]
            },
            "annotations": {
                "title": "Move cursor",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "click",
            "description":
                "Click a mouse button. Omit x/y to click wherever the pointer already is. \
                 ALWAYS pass `window` — focus can change between turns (a permission prompt is \
                 enough to steal it) and without it input goes to whatever happens to be \
                 focused now, not what you last looked at. With `window` set, the server \
                 focuses and verifies first, and refuses rather than misdeliver.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                    "button": { "type": "string", "enum": ["left", "right", "middle"] },
                    "window": { "type": "string", "description": WINDOW_ARG }
                }
            },
            "annotations": {
                "title": "Click",
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            }
        },
        {
            "name": "type_text",
            "description":
                "Type literal text. ALWAYS pass `window` — focus can change between turns (a \
                 permission prompt is enough to steal it) and without it input goes to \
                 whatever happens to be focused now, not what you last looked at. With \
                 `window` set, the server focuses and verifies first, and refuses rather than \
                 misdeliver.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "window": { "type": "string", "description": WINDOW_ARG }
                },
                "required": ["text"]
            },
            "annotations": {
                "title": "Type text",
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            }
        },
        {
            "name": "key",
            "description":
                "Send a key chord such as 'ctrl+c', 'super+Return', or 'Escape'. \
                 Modifiers: ctrl, shift, alt, super. The final key is an X11 keysym. \
                 ALWAYS pass `window` — focus can change between turns (a permission prompt is \
                 enough to steal it) and without it input goes to whatever happens to be \
                 focused now, not what you last looked at. With `window` set, the server \
                 focuses and verifies first, and refuses rather than misdeliver.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chord": { "type": "string" },
                    "window": { "type": "string", "description": WINDOW_ARG }
                },
                "required": ["chord"]
            },
            "annotations": {
                "title": "Key chord",
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            }
        },
        {
            "name": "scroll",
            "description":
                "Scroll the surface under the pointer. ALWAYS pass `window` — focus can change \
                 between turns (a permission prompt is enough to steal it) and without it \
                 input goes to whatever happens to be focused now, not what you last looked \
                 at. With `window` set, the server focuses and verifies first, and refuses \
                 rather than misdeliver.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                    "amount": { "type": "integer", "description": "Defaults to 3." },
                    "window": { "type": "string", "description": WINDOW_ARG }
                },
                "required": ["direction"]
            },
            "annotations": {
                "title": "Scroll",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": true
            }
        },
        {
            "name": "drag",
            "description":
                "Press a mouse button at one point, sweep the pointer to another, \
                 release: drag-and-drop, sliders, text selection, rubber-band \
                 selection. ALWAYS pass `window` — focus can change between turns \
                 and the press must land on the window you looked at. Needs \
                 ydotool (the only backend with separate press/release); doctor \
                 shows whether it is available.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from_x": { "type": "integer" },
                    "from_y": { "type": "integer" },
                    "to_x": { "type": "integer" },
                    "to_y": { "type": "integer" },
                    "button": { "type": "string", "enum": ["left", "right", "middle"] },
                    "window": { "type": "string", "description": WINDOW_ARG }
                },
                "required": ["from_x", "from_y", "to_x", "to_y"]
            },
            "annotations": {
                "title": "Drag",
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            }
        },
        {
            "name": "move_window",
            "description":
                "Place a window's top-left corner at an absolute position. Only \
                 floating windows move freely; a tiled window ignores this, and \
                 the result reports where the window actually ended up either \
                 way. Useful for arranging windows before multi-window work or \
                 getting one out of the way of another.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "address": { "type": "string", "description": "Window address from list_windows." },
                    "x": { "type": "integer" },
                    "y": { "type": "integer" }
                },
                "required": ["address", "x", "y"]
            },
            "annotations": {
                "title": "Move window",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "resize_window",
            "description":
                "Resize a window to an exact size. Floating windows resize \
                 exactly; on a tiled window the compositor adjusts the layout \
                 split as far as it can, and the result reports the actual \
                 resulting size either way. Useful before screenshots of apps \
                 that hide UI at small sizes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "address": { "type": "string", "description": "Window address from list_windows." },
                    "width": { "type": "integer" },
                    "height": { "type": "integer" }
                },
                "required": ["address", "width", "height"]
            },
            "annotations": {
                "title": "Resize window",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "ui_tree",
            "description":
                "Read a window's UI semantically via accessibility (AT-SPI): every \
                 element's role, name, state, absolute centre coordinates, and \
                 available actions, as an indented tree. STRONGLY PREFER this over \
                 screenshots when it works — it is exact where pixel-guessing is \
                 approximate, far cheaper in tokens, and its «id» tokens feed \
                 element_action / element_set_text, which need no pointer at all. \
                 Not every app exposes accessibility (doctor shows per-window \
                 coverage); fall back to screenshot when this errors.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": { "type": "string", "description": "Window address from list_windows. Defaults to the focused window." },
                    "depth": { "type": "integer", "description": "Max tree depth to descend. Defaults to 12." },
                    "all": { "type": "boolean", "description": "Include unnamed structural containers normally elided. Default false." }
                }
            },
            "annotations": {
                "title": "UI tree (semantic)",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "find_element",
            "description":
                "Search accessible UI elements by name substring and/or role \
                 ('push button', 'text', 'menu item', ...). Searches one window, or \
                 every accessible window when `window` is omitted. Returns matches \
                 with absolute centre coordinates, states, actions, and «id» tokens \
                 for the element_* tools. Cheaper and more precise than reading a \
                 full ui_tree when you know what you are looking for.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Case-insensitive substring of the element's accessible name. Empty matches every element (filter by role)." },
                    "role": { "type": "string", "description": "Exact role name, e.g. 'push button', 'text', 'check box'." },
                    "window": { "type": "string", "description": "Restrict to this window address." }
                },
                "required": ["query"]
            },
            "annotations": {
                "title": "Find UI element",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "element_action",
            "description":
                "Invoke an element's own action ('click', 'press', 'toggle', ...) by \
                 «id» from ui_tree/find_element. This asks the app directly — no \
                 pointer movement, no focus dance, no input tools needed, and it \
                 works on off-screen elements. Prefer it over coordinate clicks \
                 whenever the element exposes an action. Omit `action` to invoke \
                 the element's primary (first) action.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "element": { "type": "string", "description": "Element «id» token." },
                    "action": { "type": "string", "description": "Action name; defaults to the first listed." }
                },
                "required": ["element"]
            },
            "annotations": {
                "title": "Invoke element action",
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            }
        },
        {
            "name": "element_read",
            "description":
                "Read an element's full text content or numeric value by «id». Use \
                 for text fields, documents, sliders — exact text, no OCR-from-\
                 screenshot guessing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "element": { "type": "string", "description": "Element «id» token." }
                },
                "required": ["element"]
            },
            "annotations": {
                "title": "Read element",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "element_set_text",
            "description":
                "Replace an editable element's text contents in one call by «id» — \
                 atomic and exact where type_text is keystroke-by-keystroke. Only \
                 works on elements marked {set_text} in ui_tree. Verify with \
                 element_read afterwards.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "element": { "type": "string", "description": "Element «id» token." },
                    "text": { "type": "string" }
                },
                "required": ["element", "text"]
            },
            "annotations": {
                "title": "Set element text",
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": true,
                "openWorldHint": true
            }
        },
        {
            "name": "element_focus",
            "description":
                "Give an element keyboard focus by «id», e.g. a text field before \
                 type_text or key.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "element": { "type": "string", "description": "Element «id» token." }
                },
                "required": ["element"]
            },
            "annotations": {
                "title": "Focus element",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "app_notes",
            "description":
                "Explicitly revisit saved application notes, or call with no `app` \
                 to list every app with notes. Normal GUI work does not require a \
                 manual read: hyprhands injects notes automatically on the first \
                 encounter with each app in an MCP session. Do not call this \
                 proactively. Notes are version-stamped \
                 against the app binary: CURRENT means trust them, STALE means verify \
                 them, and UNVERIFIABLE means the app is not running.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "app": { "type": "string", "description": "App class as shown in list_windows, e.g. 'nautilus'. Omit to list all." }
                }
            },
            "annotations": {
                "title": "Read app notes",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "app_notes_write",
            "description":
                "Save (replace) your notes about an application for future \
                 sessions: where key controls live, which workflows succeed, quirks \
                 and gotchas. A HYPRHANDS MEMORY CHECKPOINT in another tool result \
                 requires you to consider this write after the workflow is verified \
                 and before your final response. Write only when reusable facts were \
                 learned; every claim must be backed by a returned tool result or \
                 existing CURRENT notes. Exclude untested background knowledge, user \
                 content, secrets, guesses, task-specific test values, and transient \
                 environment/session state (dependency or bus availability, open content, \
                 current media, focus, geometry, and capability failures). Generalize \
                 examples with placeholders. REWRITE notes \
                 when facts changed—the content is a full replacement. The server \
                 stamps the running binary's identity so staleness is detected \
                 automatically when the app updates; superseded notes are archived. \
                 Content should be dense and factual — element names, menu paths, \
                 sequences that worked.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "app": { "type": "string", "description": "App class as shown in list_windows." },
                    "content": { "type": "string", "description": "Full replacement notes (markdown)." },
                    "version": { "type": "string", "description": "Human-readable app version if you know it (e.g. from --version). Auto-detected from Nix store paths when omitted." }
                },
                "required": ["app", "content"]
            },
            "annotations": {
                "title": "Write app notes",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "focus_window",
            "description": "Focus a window by the address reported by list_windows.",
            "inputSchema": {
                "type": "object",
                "properties": { "address": { "type": "string" } },
                "required": ["address"]
            },
            "annotations": {
                "title": "Focus window",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "launch",
            "description":
                "Launch an application and wait for its window: the compositor's \
                 event stream reports what mapped, so the result includes the new \
                 window's address, class and title directly — no list_windows \
                 polling needed. When the accessibility bus is up, the app is \
                 started with accessibility enabled for GTK/Qt automatically; for \
                 Electron/Chromium apps ALSO append --force-renderer-accessibility \
                 to the command — that is what hydrates their full ui_tree. Prefer \
                 relaunching an app accessibly over driving an inaccessible \
                 instance by pixels.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "wait_seconds": {
                        "type": "integer",
                        "description":
                            "How long to wait for a window to map (default 8, max 30). \
                             0 returns immediately. Timing out is not failure — \
                             single-instance and windowless commands map nothing."
                    }
                },
                "required": ["command"]
            },
            "annotations": {
                "title": "Launch app",
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            }
        }
    ])
}

#[derive(Default)]
struct MemorySession {
    /// App classes whose saved notes have already been pushed into this MCP
    /// session. This is deliberately session-local: a new agent session gets
    /// a fresh injection without maintaining any hidden database state.
    surfaced_apps: HashSet<String>,
}

struct MemoryDecoration {
    before: Option<String>,
    after: Option<String>,
}

impl MemorySession {
    /// Full-replacement writes must not silently discard a record the model
    /// has never seen. Return the existing memory and require a deliberate
    /// retry after it has entered this session's context.
    fn guard_write(&mut self, executor: &Executor, action: &Action) -> Option<Value> {
        let Action::AppNotesWrite { app, .. } = action else {
            return None;
        };
        let key = app.to_ascii_lowercase();
        if self.surfaced_apps.contains(&key) || !notes::exists(app) {
            return None;
        }

        self.surfaced_apps.insert(key);
        let existing = notes::read(executor.compositor(), &Some(app.clone()))
            .unwrap_or_else(|e| format!("The existing record could not be loaded safely: {e}"));
        Some(json!({
            "content": [{
                "type": "text",
                "text": format!(
                    "HYPRHANDS MEMORY MERGE REQUIRED — write not performed for {app}. \
                     app_notes_write replaces the whole record, and this session had not \
                     reviewed the existing notes. Merge any still-valid facts with your new, \
                     verified knowledge, then call app_notes_write again with the complete \
                     replacement.\n\n{existing}"
                )
            }],
            "isError": true
        }))
    }

    fn decorate(
        &mut self,
        executor: &Executor,
        action: &Action,
        app: Option<String>,
    ) -> MemoryDecoration {
        match action {
            // Explicit reads already return the memory, so only suppress a
            // duplicate automatic injection later in the same session.
            Action::AppNotes { app: Some(app) } => {
                self.surfaced_apps.insert(app.to_ascii_lowercase());
                return MemoryDecoration {
                    before: None,
                    after: None,
                };
            }
            // A successful write is itself the requested checkpoint.
            Action::AppNotesWrite { app, .. } => {
                self.surfaced_apps.insert(app.to_ascii_lowercase());
                return MemoryDecoration {
                    before: None,
                    after: None,
                };
            }
            Action::AppNotes { app: None } => {
                return MemoryDecoration {
                    before: None,
                    after: None,
                };
            }
            _ => {}
        }

        let Some(app) = app else {
            return MemoryDecoration {
                before: None,
                after: None,
            };
        };

        let key = app.to_ascii_lowercase();
        let first_encounter = self.surfaced_apps.insert(key);
        let before = first_encounter.then(|| {
            let body =
                notes::read(executor.compositor(), &Some(app.clone())).unwrap_or_else(|_| {
                    format!(
                        "No saved notes exist for {app}. Explore first; save only reusable, \
                     verified UI knowledge if you learn any."
                    )
                });
            format!("HYPRHANDS MEMORY — automatically loaded for {app}:\n{body}")
        });

        let after = is_memory_bearing(action).then(|| {
            format!(
                "HYPRHANDS MEMORY CHECKPOINT — {app}: continue the unfinished workflow. \
                 Before your final response, call app_notes_write with full replacement notes \
                 if you learned reusable, verified UI facts. Never save user content, secrets, \
                 or guesses."
            )
        });

        MemoryDecoration { before, after }
    }
}

fn active_class(executor: &Executor) -> Option<String> {
    executor
        .compositor()
        .active_window()
        .ok()
        .flatten()
        .map(|w| w.class)
}

fn addressed_class(executor: &Executor, address: &str) -> Option<String> {
    executor
        .compositor()
        .window_by_address(address)
        .ok()
        .map(|w| w.class)
}

/// Resolve the application an action is about to observe or affect. Doing this
/// before execution is load-bearing for close actions: afterward the target
/// window may already be gone and focus may have moved to an unrelated app.
fn action_app_before(executor: &Executor, action: &Action) -> Option<String> {
    match action {
        Action::Launch { .. } => None,
        // This observes every app at once. Associating it with whichever app
        // happened to be focused injected unrelated memory into many tasks.
        Action::ListWindows => None,
        Action::Screenshot { target, .. } => match target {
            CaptureTarget::ActiveWindow => active_class(executor),
            CaptureTarget::Window(address) => addressed_class(executor, address),
            CaptureTarget::All | CaptureTarget::Monitor(_) | CaptureTarget::Region(_) => None,
        },
        Action::Click { window, .. }
        | Action::TypeText { window, .. }
        | Action::Key { window, .. }
        | Action::Scroll { window, .. }
        | Action::Drag { window, .. }
        | Action::UiTree { window, .. } => window
            .as_deref()
            .and_then(|address| addressed_class(executor, address))
            .or_else(|| active_class(executor)),
        Action::FindElement { window, .. } => window
            .as_deref()
            .and_then(|address| addressed_class(executor, address)),
        Action::FocusWindow(address)
        | Action::MoveWindow { address, .. }
        | Action::ResizeWindow { address, .. } => addressed_class(executor, address),
        // AT-SPI element ids do not carry a Hyprland address. Actions normally
        // focus their owning app, so the active class is the safest association.
        Action::ElementAction { .. }
        | Action::ElementRead { .. }
        | Action::ElementSetText { .. }
        | Action::ElementFocus { .. } => active_class(executor),
        Action::Doctor
        | Action::CursorPosition
        | Action::MoveCursor(_)
        | Action::AppNotes { .. }
        | Action::AppNotesWrite { .. } => None,
    }
}

fn launched_app(observation: &Observation) -> Option<String> {
    match observation {
        Observation::TextForApps { apps, .. } => apps.first().cloned(),
        _ => None,
    }
}

fn is_memory_bearing(action: &Action) -> bool {
    matches!(
        action,
        Action::Screenshot { .. }
            | Action::Click { .. }
            | Action::TypeText { .. }
            | Action::Key { .. }
            | Action::Scroll { .. }
            | Action::Drag { .. }
            | Action::Launch { .. }
            | Action::UiTree { .. }
            | Action::FindElement { .. }
            | Action::ElementAction { .. }
            | Action::ElementRead { .. }
            | Action::ElementSetText { .. }
            | Action::ElementFocus { .. }
    )
}

fn call_tool(executor: &Executor, memory: &mut MemorySession, params: &Value) -> Value {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let action = match parse_action(name, &args) {
        Ok(action) => action,
        Err(e) => {
            return json!({
                "content": [{ "type": "text", "text": e.to_string() }],
                "isError": true
            });
        }
    };

    if let Some(response) = memory.guard_write(executor, &action) {
        return response;
    }

    let app_before = action_app_before(executor, &action);
    if let Some(app) = &app_before {
        // Capture identity while the target still exists. A successful key or
        // click can close the final window before the later memory write.
        notes::observe_running(executor.compositor(), app);
    }
    match executor.execute(&action) {
        Ok(observation) => {
            let app = if matches!(action, Action::Launch { .. }) {
                launched_app(&observation)
            } else {
                app_before
            };
            if let Some(app) = &app {
                // Launches can only be attributed after their observation.
                notes::observe_running(executor.compositor(), app);
            }
            let decoration = memory.decorate(executor, &action, app);
            encode(observation, decoration)
        }
        // Tool failures come back as content with isError, not as a JSON-RPC
        // error — the model needs to read the hint and route around it.
        Err(e) => json!({
            "content": [{ "type": "text", "text": e.to_string() }],
            "isError": true
        }),
    }
}

fn encode(observation: Observation, memory: MemoryDecoration) -> Value {
    let mut content = Vec::new();
    if let Some(text) = memory.before {
        content.push(json!({ "type": "text", "text": text }));
    }
    match observation {
        Observation::Text(text) => content.push(json!({ "type": "text", "text": text })),
        Observation::TextForApps { text, .. } => {
            content.push(json!({ "type": "text", "text": text }))
        }
        Observation::Image { png, note } => {
            content.push(json!({ "type": "text", "text": note }));
            content.push(json!({
                "type": "image",
                "data": BASE64.encode(&png),
                "mimeType": "image/png"
            }));
        }
    }
    if let Some(text) = memory.after {
        content.push(json!({ "type": "text", "text": text }));
    }
    json!({ "content": content, "isError": false })
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

fn need_i32(args: &Value, key: &str) -> Result<i32> {
    args.get(key)
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
        .ok_or_else(|| Error::new(format!("missing required integer argument {key:?}")))
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn need_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::new(format!("missing required string argument {key:?}")))
}

fn parse_action(name: &str, args: &Value) -> Result<Action> {
    match name {
        "screenshot" => {
            let target = args
                .get("target")
                .and_then(|v| v.as_str())
                .unwrap_or("active_window");
            let target = match target {
                "all" => CaptureTarget::All,
                "active_window" => CaptureTarget::ActiveWindow,
                "window" => CaptureTarget::Window(need_str(args, "address")?.to_string()),
                "monitor" => CaptureTarget::Monitor(need_str(args, "monitor")?.to_string()),
                "region" => CaptureTarget::Region(Rect {
                    x: need_i32(args, "x")?,
                    y: need_i32(args, "y")?,
                    w: need_i32(args, "width")?,
                    h: need_i32(args, "height")?,
                }),
                other => {
                    return Err(Error::new(format!("unknown screenshot target {other:?}")));
                }
            };
            Ok(Action::Screenshot {
                target,
                scale: args.get("scale").and_then(|v| v.as_f64()),
            })
        }

        "doctor" => Ok(Action::Doctor),
        "list_windows" => Ok(Action::ListWindows),
        "cursor_position" => Ok(Action::CursorPosition),

        "move_cursor" => Ok(Action::MoveCursor(Point {
            x: need_i32(args, "x")?,
            y: need_i32(args, "y")?,
        })),

        "click" => {
            let at = match (args.get("x"), args.get("y")) {
                (Some(_), Some(_)) => Some(Point {
                    x: need_i32(args, "x")?,
                    y: need_i32(args, "y")?,
                }),
                (None, None) => None,
                _ => {
                    return Err(Error::new("click needs both x and y, or neither"));
                }
            };
            let button = match args.get("button").and_then(|b| b.as_str()) {
                Some(b) => Button::parse(b)?,
                None => Button::Left,
            };
            Ok(Action::Click {
                at,
                button,
                window: opt_str(args, "window"),
            })
        }

        "type_text" => Ok(Action::TypeText {
            text: need_str(args, "text")?.to_string(),
            window: opt_str(args, "window"),
        }),

        "key" => Ok(Action::Key {
            chord: need_str(args, "chord")?.to_string(),
            window: opt_str(args, "window"),
        }),

        "scroll" => Ok(Action::Scroll {
            direction: ScrollDirection::parse(need_str(args, "direction")?)?,
            amount: args
                .get("amount")
                .and_then(|v| v.as_i64())
                .unwrap_or(3)
                .max(1) as i32,
            window: opt_str(args, "window"),
        }),

        "drag" => {
            let button = match args.get("button").and_then(|b| b.as_str()) {
                Some(b) => Button::parse(b)?,
                None => Button::Left,
            };
            Ok(Action::Drag {
                from: Point {
                    x: need_i32(args, "from_x")?,
                    y: need_i32(args, "from_y")?,
                },
                to: Point {
                    x: need_i32(args, "to_x")?,
                    y: need_i32(args, "to_y")?,
                },
                button,
                window: opt_str(args, "window"),
            })
        }

        "move_window" => Ok(Action::MoveWindow {
            address: need_str(args, "address")?.to_string(),
            to: Point {
                x: need_i32(args, "x")?,
                y: need_i32(args, "y")?,
            },
        }),

        "resize_window" => Ok(Action::ResizeWindow {
            address: need_str(args, "address")?.to_string(),
            w: need_i32(args, "width")?,
            h: need_i32(args, "height")?,
        }),

        "focus_window" => Ok(Action::FocusWindow(need_str(args, "address")?.to_string())),
        "launch" => Ok(Action::Launch {
            command: need_str(args, "command")?.to_string(),
            wait_seconds: args
                .get("wait_seconds")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
        }),

        "ui_tree" => Ok(Action::UiTree {
            window: opt_str(args, "window"),
            depth: args.get("depth").and_then(|v| v.as_u64()).map(|d| d as u32),
            all: args.get("all").and_then(|v| v.as_bool()).unwrap_or(false),
        }),
        "find_element" => Ok(Action::FindElement {
            query: need_str(args, "query")?.to_string(),
            role: opt_str(args, "role"),
            window: opt_str(args, "window"),
        }),
        "element_action" => Ok(Action::ElementAction {
            element: need_str(args, "element")?.to_string(),
            action: opt_str(args, "action"),
        }),
        "element_read" => Ok(Action::ElementRead {
            element: need_str(args, "element")?.to_string(),
        }),
        "element_set_text" => Ok(Action::ElementSetText {
            element: need_str(args, "element")?.to_string(),
            text: need_str(args, "text")?.to_string(),
        }),
        "element_focus" => Ok(Action::ElementFocus {
            element: need_str(args, "element")?.to_string(),
        }),
        "app_notes" => Ok(Action::AppNotes {
            app: opt_str(args, "app"),
        }),
        "app_notes_write" => Ok(Action::AppNotesWrite {
            app: need_str(args, "app")?.to_string(),
            content: need_str(args, "content")?.to_string(),
            version: opt_str(args, "version"),
        }),

        other => Err(Error::new(format!("unknown tool {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_advertises_the_memory_lifecycle() {
        let response = initialize(&json!({ "protocolVersion": "test-version" }));
        assert_eq!(response["protocolVersion"], "test-version");
        let instructions = response["instructions"].as_str().unwrap();
        assert!(instructions.contains("injected automatically"));
        assert!(instructions.contains("do not call app_notes proactively"));
        assert!(instructions.contains("app_notes_write"));
        assert!(instructions.contains("Do not record user-entered content"));
    }

    #[test]
    fn memory_bearing_actions_are_limited_to_gui_learning() {
        assert!(is_memory_bearing(&Action::Screenshot {
            target: CaptureTarget::ActiveWindow,
            scale: None,
        }));
        assert!(is_memory_bearing(&Action::Key {
            chord: "ctrl+s".into(),
            window: Some("0x1".into()),
        }));
        assert!(!is_memory_bearing(&Action::ListWindows));
        assert!(!is_memory_bearing(&Action::AppNotes { app: None }));
    }

    #[test]
    fn memory_wraps_an_observation_in_stable_order() {
        let encoded = encode(
            Observation::text("tool result"),
            MemoryDecoration {
                before: Some("saved context".into()),
                after: Some("write checkpoint".into()),
            },
        );
        let content = encoded["content"].as_array().unwrap();
        assert_eq!(content[0]["text"], "saved context");
        assert_eq!(content[1]["text"], "tool result");
        assert_eq!(content[2]["text"], "write checkpoint");
        assert_eq!(encoded["isError"], false);
    }
}
