# hyprhands

Computer use for Hyprland — an MCP server giving AI agents eyes and hands on
your Wayland desktop.

Screenshots, window state, focus, clicks, typing, key chords. Works with any
MCP-capable harness — Claude Code, Codex, Cursor — because the executor core
knows nothing about models.

**No ydotool, no root, no portals required.** The core path drives Hyprland's
own IPC, so screenshots, window enumeration, focus, cursor movement, key chords
and app launching all work with nothing installed beyond `hyprctl` and `grim`.

## Status

Early. v0.1, single developer, one machine. Honest breakdown:

| Area | State |
|---|---|
| screenshot, `list_windows`, `focus_window`, `key`, `launch`, `move_cursor` | exercised end-to-end |
| `click`, `type_text`, `scroll` | **written but unverified** — needs `wtype`/`wlrctl`, not installed on the dev box |
| multi-monitor, fractional scaling | **untested** — developed on a single 2560x1440 @ scale 1.0 |
| Sway / river | not implemented (the trait exists, the impl doesn't) |

The end-to-end check so far: driving a GPUI file manager keyboard-only to
select a file, `ctrl+c`, navigate to another directory, `ctrl+v`, verified
byte-identical by sha256 — with zero click capability available.

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

The `Action` enum is the seam. Adding a native computer-use loop later
(Anthropic's `computer_*` tool, OpenAI's equivalent) means another adapter, not
a rewrite — nothing below `exec.rs` knows what an MCP content block is.

Input backends are tried best-first:

1. **the compositor itself** (`hyprctl dispatch`) — needs nothing installed
2. **wtype / wlrctl** — ordinary Wayland clients, no root, no daemon
3. **ydotool** — works below the compositor via `/dev/uinput`, needs a daemon

Hyprland implements `virtual-keyboard-v1` and `wlr-virtual-pointer-v1`, so
tier 2 suffices here. Tier 3 exists for compositors that lack them.

## Install

```bash
git clone https://github.com/berker-z/hyprhands && cd hyprhands
cargo build --release
./target/release/hyprhands doctor
```

`doctor` reports exactly what works and what's missing. Missing capabilities
degrade gracefully — the affected tool returns an error naming what to install,
everything else keeps working.

### Runtime dependencies

| Capability | Needs | Notes |
|---|---|---|
| window state, focus, cursor, key chords, launch | `hyprctl` | ships with Hyprland |
| screenshot | `grim` | |
| click, scroll | `wlrctl` *or* `ydotool` | |
| type text | `wtype` *or* `ydotool` | |

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
> Launching the harness from a bare SSH session or a systemd unit fails at
> startup with a message saying so.

Consider pre-approving these tools in your harness. Every interactive approval
prompt takes keyboard focus, which is the exact thing the server is trying to
observe and act on — see the `window` argument below.

## Tools

| Tool | Purpose |
|---|---|
| `list_windows` | windows + geometry + monitors + cursor + focus. Cheap; call it first |
| `screenshot` | PNG of a window, monitor, region, or everything; auto-downscaled |
| `doctor` | dependency and capability report, as a tool |
| `cursor_position` | current absolute pointer position |
| `move_cursor` | absolute pointer move |
| `click` | left/right/middle, optionally at a coordinate |
| `type_text` | literal text into a window |
| `key` | chords like `ctrl+shift+t`, `super+Return` |
| `scroll` | up/down/left/right |
| `focus_window` | focus by address |
| `launch` | start an application |

### The `window` argument

`click`, `type_text`, `key` and `scroll` all take an optional `window` address.
Pass it. Input is delivered to whatever holds focus *at the instant it lands*,
which is not necessarily what the caller last observed — a notification, the
user alt-tabbing, or an approval prompt is enough to move it. With `window`
set, the server focuses the target, polls until the compositor confirms it, and
**refuses to send input** if focus never lands. Without it, input goes wherever
focus happens to be; the result still reports which window received it, so a
misdelivery is at least visible rather than silent.

### Coordinates

All actions take **absolute layout coordinates**. Screenshots report what you
need to get back to them:

- Cropped captures report their absolute top-left offset — add it.
- Captures over 1400px on the long edge are downscaled automatically (roughly a
  4x token saving) and report the factor — divide by it, then add the offset.
  Pass `scale: 1.0` when you need to read fine detail.
- Captures at or below 2576px on the long edge map 1:1 to the coordinates
  current models emit, so no further rescaling applies.

`screenshot` refuses to capture a window whose workspace isn't displayed on any
monitor. `grim -g` grabs a *screen region*, not a window, so an off-workspace
target would silently return whatever else is painted at those coordinates.
Hyprland's `mapped`/`hidden` fields do not catch this — such a window reports
`mapped: true, hidden: false` while rendering nowhere.

## Known limitations

- **`click` / `type_text` / `scroll` are unverified.** Written against `wlrctl`
  and `wtype` argument formats but never executed — the development machine has
  neither installed. Treat as alpha until someone runs them.
- **Multi-monitor and fractional scaling are untested.** `grim` captures in
  physical pixels while Hyprland reports logical coordinates. Identical at
  `scale = 1.0`; the transform is almost certainly wrong under fractional
  scaling, and `layout_bounds` for multi-output grabs has never run against a
  real second monitor.
- **`launch` reports the dispatch, not the process.** `hyprctl dispatch exec`
  returns `ok` once it has spawned the command; it cannot tell you the binary
  crashed, or that a single-instance app deferred to an existing windowless
  process. Confirm with `list_windows`.
- **No event-socket integration.** Hyprland's `socket2` would let the server
  await `openwindow` / `windowtitle` rather than have the caller sleep and
  re-poll after `launch`.
- **Screenshots are still the dominant cost** even downscaled. A full-screen
  grab at default scale is on the order of 1.5k image tokens; a native one is
  ~4.8k.
- **Focus-then-act is two IPC round trips.** The `window` argument narrows the
  race a lot but does not eliminate it in principle.
- **Hyprland only**, despite the `Compositor` trait.
- **No tests.** Verification so far has been manual, against one desktop.

## Roadmap

- [ ] Verify `click` / `type_text` / `scroll` against real `wtype` + `wlrctl`
- [ ] `socket2` event integration — let `launch` await window map, and `key`
      await a title change, instead of callers sleeping
- [ ] Sway implementation behind the existing `Compositor` trait
- [ ] Correct logical↔physical transform, validated on a multi-monitor
      fractional-scaling setup
- [ ] JPEG output option for further token reduction on photographic content
- [ ] Native computer-use adapter (Anthropic / OpenAI tool schemas) alongside
      `mcp.rs`, reusing the same `Action` core
- [ ] Integration tests against a headless compositor

## Acknowledgements

[`agent-sh/computer-use-linux`](https://github.com/agent-sh/computer-use-linux)
solves a broader version of this problem — AT-SPI, GNOME Shell, portals and
`ydotool` across GNOME, KDE, Hyprland, i3 and COSMIC — and is considerably more
mature. Two ideas here come from reading it:

- **Downscale, but report the conversion factor.** Their screenshot results
  return `coordinate_width` / `coordinate_height` / `scale` so callers can map a
  downscaled preview back to desktop pixels. `hyprhands` does the same thing in
  prose in the accompanying text block, which is what makes automatic
  downscaling safe rather than a silent source of misclicks.
- **`doctor` as a tool, not just a CLI subcommand.** Exposing the capability
  report over MCP lets the agent discover what it can actually do, instead of
  finding out via a failed action.

Where they differ: for a window that isn't currently rendered, they raise it to
the front and capture; `hyprhands` refuses and explains. Theirs is more useful,
ours has no side effects on your desktop layout. Reasonable people could pick
either.

If you want broad desktop coverage, use theirs. `hyprhands` is for people who
want a small Hyprland-native binary with no daemon and no portal prompts.

## License

MIT
