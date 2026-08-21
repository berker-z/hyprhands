# hyprhands

Computer use for Hyprland: an MCP server that gives AI agents eyes and hands
on your Wayland desktop.

The basics are screenshots, window state, focus, clicks, typing, and key
chords. Where an app exposes an accessibility tree, the agent can skip pixels
entirely: it reads roles, names and exact coordinates, and invokes an
element's own actions without touching the pointer. Any MCP-capable harness
works (Claude Code, Codex, Cursor) because the executor core knows nothing
about models.

It also keeps versioned memory per app. Agents can carry what they learned
between sessions, while staleness checks flag notes that may no longer match
the running version.

No ydotool, no root, no portals. The core path drives Hyprland's own IPC, so
window enumeration, focus, cursor movement, key chords, launching and
screenshots need nothing installed beyond `hyprctl` and `grim`.

## Status

Early. v0.1, single developer, one machine.

| Area | State |
|---|---|
| screenshot, window state, focus, keys, cursor | verified end to end |
| `click`, `type_text`, `scroll` | verified via `wlrctl` and `wtype` |
| semantic tools (`ui_tree`, `element_*`) | verified against GTK4, Chromium, and AccessKit apps |
| `app_notes` versioning | verified, including staleness and history |
| `launch` awaiting the window | verified |
| multi-monitor, fractional scaling | untested |
| Sway / river | not implemented |
| tests | parsing logic only |

Two end-to-end runs are worth describing. The first drove a GPUI file manager
keyboard-only (no click capability existed yet): select a file, `ctrl+c`,
navigate, `ctrl+v`, then verify the copy byte-identical by sha256. The second
drove nautilus search entirely through the accessibility tree: a semantic
click on the search toggle, a typed query, and the filtered results read back
from the tree. No screenshot was taken at any point.

## Design

```
adapters   mcp.rs                 JSON-RPC 2.0 over stdio
              ↓ Action
core       action.rs / exec.rs    provider-neutral verbs
              ↓
backends   compositor.rs          hyprctl -j, socket2 events
           capture.rs             grim
           input.rs               wtype / wlrctl / ydotool
           a11y.rs                AT-SPI over D-Bus
           notes.rs               versioned per-app memory
```

Everything meets at the `Action` enum. Adding a native computer-use loop
later (Anthropic's `computer_*` tool, OpenAI's equivalent) means writing
another adapter, not rewriting anything; nothing below `exec.rs` knows what
an MCP content block is.

Input goes through the first backend that works. The compositor itself is
tried first (`hyprctl dispatch`, nothing to install), then wtype and wlrctl,
which are ordinary Wayland clients. ydotool is the last resort: it injects
through `/dev/uinput` below the compositor and needs a daemon. Hyprland
implements `virtual-keyboard-v1` and `wlr-virtual-pointer-v1`, so the middle
tier covers everything here; the ydotool path exists for compositors that
lack those protocols.

## Install

With Nix, the runtime tools come along and there is nothing else to set up:

```bash
nix run github:berker-z/hyprhands -- doctor
# or into a profile / your flake inputs:
nix profile install github:berker-z/hyprhands
```

The packaged binary is wrapped so `grim`, `wlrctl` and `wtype` are on its
PATH, as a suffix, so your system versions win if you have them. `hyprctl` is
deliberately not bundled. It has to come from the running Hyprland session,
otherwise the two can drift apart on version.

From source on any distro:

```bash
git clone https://github.com/berker-z/hyprhands && cd hyprhands
cargo build --release
./target/release/hyprhands doctor
```

`doctor` reports what works and what is missing. A missing tool degrades
gracefully: the affected MCP tool returns an error naming the dependency, and
everything else keeps working.

Building from source you install the runtime tools yourself:

| Distro | Command |
|---|---|
| NixOS (non-flake) | `environment.systemPackages = with pkgs; [ grim wlrctl wtype ];` |
| Arch | `pacman -S grim wlrctl wtype` |
| Fedora | `dnf install grim wtype` (`wlrctl` is not packaged; build it or use `ydotool`) |
| Debian/Ubuntu | `apt install grim wtype` (`wlrctl` likewise from source or `ydotool`) |

For the semantic tools you also want the accessibility bus running; see
[Semantic UI](#semantic-ui-at-spi).

## Wiring it up

The server is a subprocess speaking JSON-RPC over stdio. No ports, no daemon.

Claude Code:

```bash
claude mcp add hyprhands -- /absolute/path/to/hyprhands mcp
```

Codex, in `~/.codex/config.toml`:

```toml
[mcp_servers.hyprhands]
command = "/absolute/path/to/hyprhands"
args = ["mcp"]
```

It must run inside your graphical session so it inherits `WAYLAND_DISPLAY`,
`HYPRLAND_INSTANCE_SIGNATURE`, and `XDG_RUNTIME_DIR`. Launching the harness
from a bare SSH session or a systemd unit fails at startup with a message
saying so.

Consider pre-approving these tools in your harness. Every interactive
approval prompt takes keyboard focus, which is the exact thing the server is
trying to observe and act on.

## Tools

| Tool | Purpose |
|---|---|
| `list_windows` | windows, geometry, monitors, cursor, focus. Cheap; call it first |
| `screenshot` | PNG of a window, monitor, region, or everything; auto-downscaled |
| `doctor` | dependency and capability report, as a tool |
| `cursor_position` | current absolute pointer position |
| `move_cursor` | absolute pointer move |
| `click` | left/right/middle, optionally at a coordinate |
| `type_text` | literal text into a window |
| `key` | chords like `ctrl+shift+t`, `super+Return` |
| `scroll` | up/down/left/right |
| `focus_window` | focus by address |
| `launch` | start an app and wait for its window; returns the mapped address |
| `ui_tree` | a window's UI as a tree of roles, names, states, coordinates, actions |
| `find_element` | search elements by name or role, in one window or all of them |
| `element_action` | invoke an element's own action (`click`, `toggle`, ...) |
| `element_read` | exact text or value of an element |
| `element_set_text` | replace an editable element's contents in one call |
| `element_focus` | give an element keyboard focus |
| `app_notes` | read version-stamped notes about an app from past sessions |
| `app_notes_write` | save what was learned about an app |

`click`, `type_text`, `key` and `scroll` all take an optional `window`
address. Pass it. Input lands on whatever holds focus at that instant, which
is not necessarily what the caller last observed; a notification or the user
alt-tabbing is enough to move it. With `window` set, the server focuses the
target, polls until the compositor confirms, and refuses to send input if
focus never lands. Without it, input goes wherever focus happens to be. The
result still reports which window received it, so a misdelivery is at least
visible.

All actions take absolute layout coordinates, and screenshots report what you
need to map back to them. Cropped captures report their absolute top-left
offset. Captures over 1400px on the long edge are downscaled (roughly a 4x
token saving) and report the factor; divide by it, then add the offset. Pass
`scale: 1.0` when you need fine detail. At or below 2576px on the long edge,
image coordinates map 1:1 to what current models emit.

`screenshot` refuses to capture a window whose workspace is not displayed on
any monitor. `grim -g` grabs a screen region, not a window, so an
off-workspace target would silently return whatever else is painted at those
coordinates. Hyprland's `mapped` and `hidden` fields do not catch this; such
a window reports `mapped: true, hidden: false` while rendering nowhere.

`launch` subscribes to Hyprland's event socket before dispatching, then waits
(default 8s, tunable) for `openwindow` events and returns the new window's
address, class and title directly. No sleep-and-poll loop on the caller's
side. A timeout is reported as information, not an error, because
single-instance apps defer to an existing process and windowless commands map
nothing.

## Semantic UI (AT-SPI)

Pixels are the fallback, not the plan. Where an app implements AT-SPI, the
freedesktop accessibility protocol, `ui_tree` reads the UI as meaning: each
element's role, accessible name, state, and the actions it will perform on
request. `element_action` then asks the app to activate an element directly.
No screenshot tokens, no pixel guessing, no input tool, and it works on
elements that are scrolled out of view.

Coordinates need care on Wayland. A Wayland client cannot know its own
absolute position, so AT-SPI screen coordinates are garbage on principle.
hyprhands instead asks for window-relative extents, normalises them against
the toplevel frame's own reported origin (which cancels client-side
decoration offsets), and adds the window position Hyprland reports. The
result is an absolute layout coordinate that `click` and `move_cursor` accept
unmodified. Apps are matched to Hyprland windows by PID, frames to windows by
title, and `doctor` shows exactly which open windows are accessible.

Coverage is per-toolkit. `launch` does the enabling where it can: when the
bus is up it wraps commands with the Qt/GTK environment
(`QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1`, `NO_AT_BRIDGE` unset) and advertises
the screen-reader flag on `org.a11y.Status`, and the server registers AT-SPI
event listeners so apps see a live assistive technology rather than a
drive-by client.

- GTK3/GTK4 work out of the box once the bus is up.
- Qt is covered by `launch`'s env injection.
- Electron and Chromium register from the screen-reader flag (frame and
  title), but hydrating the full renderer tree additionally needs
  `--force-renderer-accessibility` appended to the launch command. Verified
  against Obsidian. Some hardened forks strip accessibility entirely.
- AccessKit apps (GPUI/Zed, egui, Slint) work. They omit `GetRoleName`, which
  hyprhands papers over with a numeric role lookup.
- Terminals and games usually expose nothing. Use screenshots.

Apps register only at startup, so restart an app after enabling any of this.

The bus itself: most GNOME and KDE systems already run it. On NixOS set
`services.gnome.at-spi2-core.enable = true` and make sure `NO_AT_BRIDGE=1` is
not exported. Elsewhere install `at-spi2-core`. Without the bus the semantic
tools return an error naming the fix, and the screenshot flow is unaffected.

## Per-app memory

Driving a GUI is mostly rediscovery: where the search field is, which menu
hides the export button, which dialog steals focus. `app_notes_write` lets
the agent record what it learned about a program, and `app_notes` replays it
next session, so session two starts warm.

The twist is versioning. Notes are stamped with the running binary's identity
(resolved `/proc/<pid>/exe`, size, mtime; on Nix the store path alone encodes
the version, and a human-readable version is extracted from it). On read the
stamp is compared against what is running now. CURRENT means the same binary,
trust the notes. STALE means the app was updated since (`0.48.1 -> 0.48.2`),
so the notes come back framed as hypotheses with an instruction to verify and
rewrite; the superseded notes are archived under `history/`. UNVERIFIABLE
means the app is not running and no claim is made either way.

Notes live in `$XDG_DATA_HOME/hyprhands/notes/<app>/` as plain markdown plus
a small `meta.json`. Greppable, editable, deletable by hand.

## Known limitations

- AT-SPI coverage is what apps give you. Even covered toolkits have gaps:
  GTK4's search entry advertises `editable` state but implements no
  `EditableText` interface. The tools report this honestly and the error
  names the fallback. Large trees (browsers) are walked with a node budget
  and truncate with a notice rather than stall.
- Element ids are AT-SPI object references, valid for the element's lifetime
  and not across app restarts. Re-run `ui_tree` rather than caching ids in
  notes.
- App-notes fingerprinting keys on the window's process. Wrapper scripts can
  make `/proc/<pid>/exe` point somewhere version-stable; the agent-supplied
  `version` argument is the override.
- Multi-monitor and fractional scaling are untested. `grim` captures physical
  pixels while Hyprland reports logical coordinates. Identical at scale 1.0;
  the transform is almost certainly wrong under fractional scaling.
- `launch` confirms the window, not the process. A window mapped by a
  concurrent source at the same instant is indistinguishable from the
  launched app's, and a crash before mapping reads as a timeout. The reported
  class is the sanity check.
- Screenshots are still the dominant cost even downscaled: roughly 1.5k image
  tokens for a full-screen grab at default scale, ~4.8k native.
- Focus-then-act is two IPC round trips. The `window` argument narrows the
  race but does not eliminate it in principle.
- Hyprland only, despite the `Compositor` trait.
- Tests cover parsing (chords, socket2 events, element ids, Nix versions,
  dates), not the desktop. Everything touching a live compositor is verified
  manually, against one machine.

## Roadmap

- [ ] socket2 for input verification: let `key` and `click` optionally await
      a title or focus change as confirmation the action landed
- [ ] Sway implementation behind the existing `Compositor` trait
- [ ] Correct logical/physical transform, validated on a multi-monitor
      fractional-scaling setup
- [ ] JPEG output option for photographic content
- [ ] Native computer-use adapter (Anthropic / OpenAI tool schemas) alongside
      `mcp.rs`, reusing the same `Action` core
- [ ] Integration tests against a headless compositor

Done so far: input verified against real `wtype` and `wlrctl`; the AT-SPI
layer with compositor-anchored coordinates; versioned app notes; `launch`
that awaits `openwindow` on socket2; accessibility-aware launching verified
against Chromium and Obsidian.

## Acknowledgements

[`agent-sh/computer-use-linux`](https://github.com/agent-sh/computer-use-linux)
solves a broader version of this problem (AT-SPI, GNOME Shell, portals and
`ydotool` across GNOME, KDE, Hyprland, i3 and COSMIC) and is considerably
more mature. Two ideas here come from reading it. Their screenshot results
return `coordinate_width`, `coordinate_height` and `scale` so callers can map
a downscaled preview back to desktop pixels; hyprhands does the same thing in
prose in the accompanying text block, which is what makes automatic
downscaling safe rather than a silent source of misclicks. And exposing
`doctor` over MCP, not just as a CLI subcommand, lets the agent discover what
it can do instead of finding out through a failed action.

One place we differ: for a window that is not currently rendered, they raise
it and capture; hyprhands refuses and explains. Theirs is more useful, ours
leaves your desktop layout alone. Reasonable people could pick either.

If you want broad desktop coverage, use theirs. hyprhands is for people who
want a small Hyprland-native binary with no daemon and no portal prompts.

## License

MIT
