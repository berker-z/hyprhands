//! MCP adapter: newline-delimited JSON-RPC 2.0 over stdio.
//!
//! Hand-rolled rather than pulled from an SDK — the surface we need is small
//! and fully specified, and this keeps the dependency tree to three crates.
//! Swapping in `rmcp` later touches only this file.
//!
//! stdout is the protocol channel. Anything diagnostic MUST go to stderr or it
//! corrupts the stream.

use crate::action::{
    Action, Button, CaptureTarget, Error, Observation, Point, Rect, Result, ScrollDirection,
};
use crate::exec::Executor;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use std::io::{BufRead, Write};

const SERVER_NAME: &str = "hyprhands";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const FALLBACK_PROTOCOL: &str = "2025-06-18";

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
            "tools/call" => success(id, call_tool(&executor, &params)),
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
        "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
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
                "Launch an application. Returns immediately — the window takes a moment \
                 to map, so confirm with list_windows before interacting with it.",
            "inputSchema": {
                "type": "object",
                "properties": { "command": { "type": "string" } },
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

fn call_tool(executor: &Executor, params: &Value) -> Value {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    match parse_action(name, &args).and_then(|action| executor.execute(&action)) {
        Ok(observation) => encode(observation),
        // Tool failures come back as content with isError, not as a JSON-RPC
        // error — the model needs to read the hint and route around it.
        Err(e) => json!({
            "content": [{ "type": "text", "text": e.to_string() }],
            "isError": true
        }),
    }
}

fn encode(observation: Observation) -> Value {
    match observation {
        Observation::Text(text) => json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false
        }),
        Observation::Image { png, note } => json!({
            "content": [
                { "type": "text", "text": note },
                { "type": "image", "data": BASE64.encode(&png), "mimeType": "image/png" }
            ],
            "isError": false
        }),
    }
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
                    return Err(Error::new(format!(
                        "unknown screenshot target {other:?}"
                    )));
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
                    return Err(Error::new(
                        "click needs both x and y, or neither",
                    ));
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

        "focus_window" => Ok(Action::FocusWindow(need_str(args, "address")?.to_string())),
        "launch" => Ok(Action::Launch(need_str(args, "command")?.to_string())),

        other => Err(Error::new(format!("unknown tool {other:?}"))),
    }
}
