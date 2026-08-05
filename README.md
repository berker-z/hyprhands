# hyprhands

Computer-use executor for Wayland compositors, exposed over MCP.

Lets an agent see and drive your desktop: screenshots, window state, clicks,
typing, key chords. Works with any MCP-capable harness — Claude Code, Codex,
Cursor — because the executor core knows nothing about models.

Hyprland today. The compositor layer is a trait, so Sway and river are additive.

## Design

```
adapters   mcp.rs                 JSON-RPC 2.0 over stdio
              ↓ Action
core       action.rs / exec.rs    provider-neutral verbs
              ↓
backends   compositor.rs          hyprctl -j
           capture.rs             grim
           input.rs               wtype / wlrctl / ydotool
```

The `Action` enum is the seam. Adding a native computer-use loop later means
another adapter, not a rewrite — nothing below `exec.rs` knows what an MCP
content block is.

Input backends are tried best-first:

1. **the compositor itself** (`hyprctl dispatch`) — needs nothing installed
2. **wtype / wlrctl** — ordinary Wayland clients, no root, no daemon
3. **ydotool** — works below the compositor via `/dev/uinput`, needs a daemon

Hyprland implements `virtual-keyboard-v1` and `wlr-virtual-pointer-v1`, so
tier 2 is enough here. Tier 3 exists for compositors that lack them.

## Install

```bash
git clone https://github.com/<you>/hyprhands && cd hyprhands
cargo build --release
./target/release/hyprhands doctor
```

`doctor` tells you exactly what works and what's missing. Missing capabilities
degrade gracefully — the affected tool returns an error explaining what to
install, everything else keeps working.

### Runtime dependencies

| Capability | Needs | Notes |
|---|---|---|
| window state, focus, cursor, key chords | `hyprctl` | ships with Hyprland |
| screenshot | `grim` | |
| click, scroll | `wlrctl` *or* `ydotool` | |
| type text | `wtype` *or* `ydotool` | |

Nothing beyond `hyprctl` and `grim` is required to get a useful subset:
screenshots, window enumeration, focus, cursor movement, key chords, and app
launching all work with zero extra installs.

## Wiring it up

The server is a subprocess speaking JSON-RPC over stdio. No ports, no daemon.

**Claude Code**

```bash
claude mcp add hyprhands -- /absolute/path/to/hyprhands/target/release/hyprhands mcp
```

**Codex** — `~/.codex/config.toml`

```toml
[mcp_servers.hyprhands]
command = "/absolute/path/to/hyprhands/target/release/hyprhands"
args = ["mcp"]
```

> It must run **inside your graphical session** so it inherits
> `WAYLAND_DISPLAY`, `HYPRLAND_INSTANCE_SIGNATURE`, and `XDG_RUNTIME_DIR`.
> Launching the harness from a bare SSH session or a systemd unit will fail at
> startup with a message saying so.

## Tools

| Tool | Purpose |
|---|---|
| `list_windows` | windows + geometry + monitors + cursor + focus. Cheap; call it first |
| `screenshot` | PNG, croppable to a window, monitor, or region |
| `cursor_position` | current absolute pointer position |
| `move_cursor` | absolute pointer move |
| `click` | left/right/middle, optionally at a coordinate |
| `type_text` | literal text into the focused window |
| `key` | chords like `ctrl+shift+t`, `super+Return` |
| `scroll` | up/down/left/right |
| `focus_window` | focus by address |
| `launch` | start an application |

### Coordinates

All actions take **absolute layout coordinates**. A cropped screenshot returns
its absolute top-left offset in the accompanying text block; add it to anything
you read off the image. Captures at or below 2576px on the long edge map 1:1 to
model coordinates with no rescaling.

## Known rough edges

- **Screenshots are expensive.** A 2554x1399 window is ~4.8k image tokens.
  Cropping only helps for genuinely small windows; downscaling would be the
  real lever and isn't implemented.
- **No event-socket integration.** Hyprland's `socket2` would let the executor
  await `openwindow` / `windowtitle` instead of the caller guessing when a
  launched app is ready.
- **Multi-monitor scaling is untested.** `grim` captures in physical pixels
  while Hyprland reports logical coordinates; identical at `scale = 1`, likely
  wrong under fractional scaling.
- **Hyprland only.**

## License

MIT
