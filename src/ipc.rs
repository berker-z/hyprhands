//! App IPC over the session D-Bus: find what a running app exposes, read its
//! interfaces, and call methods on it.
//!
//! This is the rung between the CLI and AT-SPI. A CLI starts a new process;
//! D-Bus talks to the instance that is already running, with no focus, no
//! cursor and no pixels involved. Many desktop apps expose something here:
//! every media player speaks MPRIS, GTK apps export their `org.gtk.Actions`
//! (the same actions their menus trigger), and plenty of apps publish a
//! bespoke interface of their own.
//!
//! Matching a bus name to a window is done two ways, and the result says
//! which one matched: the bus daemon's view of the name owner's PID (exact),
//! and the window class appearing in the bus name (a heuristic that catches
//! apps whose D-Bus name is owned by a helper process).

use crate::action::{Error, Result};
use crate::compositor::WindowInfo;
use std::fmt::Write as _;
use std::str::FromStr as _;
use zbus::blocking::Connection;
use zbus::zvariant::{self, Signature, StructureBuilder, Value};

const DBUS: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";

/// How far below a bus name `dbus_inspect` walks looking for interfaces.
const WALK_DEPTH: usize = 4;
const WALK_BUDGET: usize = 40;

pub fn session() -> Result<Connection> {
    Connection::session().map_err(|e| {
        Error::with_hint(
            format!("cannot connect to the D-Bus session bus: {e}"),
            "hyprhands must run inside the graphical session so it inherits \
             DBUS_SESSION_BUS_ADDRESS. The app-IPC route is unavailable; the other \
             routes are unaffected.",
        )
    })
}

/// A well-known bus name associated with an app, and why.
pub struct AppName {
    pub name: String,
    pub by_pid: bool,
}

fn list_names(conn: &Connection) -> Result<Vec<String>> {
    let reply = conn
        .call_method(Some(DBUS), DBUS_PATH, Some(DBUS), "ListNames", &())
        .map_err(|e| Error::new(format!("ListNames failed: {e}")))?;
    let (names,): (Vec<String>,) = reply
        .body()
        .deserialize()
        .map_err(|e| Error::new(format!("ListNames reply: {e}")))?;
    Ok(names
        .into_iter()
        .filter(|n| !n.starts_with(':') && n != DBUS)
        .collect())
}

pub fn owner_pid(conn: &Connection, name: &str) -> Option<u32> {
    let reply = conn
        .call_method(
            Some(DBUS),
            DBUS_PATH,
            Some(DBUS),
            "GetConnectionUnixProcessID",
            &(name,),
        )
        .ok()?;
    reply.body().deserialize::<(u32,)>().ok().map(|t| t.0)
}

/// The distinctive part of a window class: `org.xfce.mousepad` → `mousepad`,
/// `io.github.berker_z.Marcel` → `marcel`. Too-short tokens match too much.
fn class_token(class: &str) -> Option<String> {
    let token = class
        .rsplit(['.', '/'])
        .next()
        .unwrap_or(class)
        .to_ascii_lowercase();
    (token.len() >= 3).then_some(token)
}

fn name_mentions(name: &str, token: &str) -> bool {
    name.to_ascii_lowercase()
        .split(['.', '-', '_'])
        .any(|part| {
            part == token
                || part
                    .strip_prefix(token)
                    .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit()))
        })
}

/// Bus names that belong to `window`'s app.
pub fn names_for(conn: &Connection, window: &WindowInfo) -> Result<Vec<AppName>> {
    let token = class_token(&window.class);
    let mut out = Vec::new();
    for name in list_names(conn)? {
        let by_pid = window.pid > 0 && owner_pid(conn, &name).map(i64::from) == Some(window.pid);
        let by_name = token.as_deref().is_some_and(|t| name_mentions(&name, t));
        if by_pid || by_name {
            out.push(AppName { name, by_pid });
        }
    }
    out.sort_by(|a, b| b.by_pid.cmp(&a.by_pid).then(a.name.cmp(&b.name)));
    Ok(out)
}

/// What a bus name is for, when it follows a convention worth naming.
pub fn describe_name(name: &str) -> Option<&'static str> {
    if name.starts_with("org.mpris.MediaPlayer2.") {
        Some(
            "MPRIS media player: org.mpris.MediaPlayer2.Player at /org/mpris/MediaPlayer2 (PlayPause, Next, Previous, Stop, Seek, OpenUri; properties PlaybackStatus, Metadata, Volume)",
        )
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Introspection
// ---------------------------------------------------------------------------

#[derive(Default, Debug)]
struct Interface {
    name: String,
    methods: Vec<String>,
    properties: Vec<String>,
    signals: Vec<String>,
}

#[derive(Default, Debug)]
struct NodeInfo {
    interfaces: Vec<Interface>,
    children: Vec<String>,
}

/// `name="x" type="s"` → value of one attribute.
fn attr<'a>(tag: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=");
    let mut rest = tag;
    while let Some(at) = rest.find(&needle) {
        // Must be a whole attribute name, not the tail of another one.
        let boundary = at == 0 || rest[..at].ends_with(char::is_whitespace);
        let after = &rest[at + needle.len()..];
        if boundary && let Some(quote) = after.chars().next().filter(|q| *q == '"' || *q == '\'') {
            let body = &after[1..];
            return body.find(quote).map(|end| &body[..end]);
        }
        rest = after;
    }
    None
}

/// Introspection XML is a fixed, shallow vocabulary; a tag scanner covers it
/// without pulling in an XML crate.
fn parse_introspection(xml: &str) -> NodeInfo {
    let mut info = NodeInfo::default();
    let mut depth_nodes = 0usize;
    let mut iface: Option<Interface> = None;
    // (kind, name, args) of the member currently open.
    let mut member: Option<(&str, String, Vec<String>)> = None;

    let mut rest = xml;
    while let Some(open) = rest.find('<') {
        let Some(close) = rest[open..].find('>') else {
            break;
        };
        let tag = &rest[open + 1..open + close];
        rest = &rest[open + close + 1..];
        if tag.starts_with('!') || tag.starts_with('?') {
            continue;
        }
        let closing = tag.starts_with('/');
        let self_closing = tag.ends_with('/');
        let body = tag.trim_start_matches('/').trim_end_matches('/');
        let kind = body.split_whitespace().next().unwrap_or("");

        match (kind, closing) {
            ("node", false) => {
                depth_nodes += 1;
                if depth_nodes == 2
                    && let Some(n) = attr(body, "name")
                {
                    info.children.push(n.to_string());
                }
                if self_closing {
                    depth_nodes -= 1;
                }
            }
            ("node", true) => depth_nodes = depth_nodes.saturating_sub(1),
            ("interface", false) if depth_nodes <= 1 => {
                let opened = Interface {
                    name: attr(body, "name").unwrap_or("?").to_string(),
                    ..Default::default()
                };
                if self_closing {
                    info.interfaces.push(opened);
                } else {
                    iface = Some(opened);
                }
            }
            ("interface", true) => {
                if let Some(i) = iface.take() {
                    info.interfaces.push(i);
                }
            }
            ("method" | "signal", false) => {
                let entry = (
                    if kind == "method" { "method" } else { "signal" },
                    attr(body, "name").unwrap_or("?").to_string(),
                    Vec::new(),
                );
                if self_closing {
                    finish_member(&mut iface, entry);
                } else {
                    member = Some(entry);
                }
            }
            ("method" | "signal", true) => {
                if let Some(entry) = member.take() {
                    finish_member(&mut iface, entry);
                }
            }
            ("arg", false) => {
                if let Some((kind, _, args)) = member.as_mut() {
                    let ty = attr(body, "type").unwrap_or("?");
                    let direction = attr(body, "direction").unwrap_or(if *kind == "signal" {
                        "out"
                    } else {
                        "in"
                    });
                    let label = match attr(body, "name") {
                        Some(n) => format!("{n}: {ty}"),
                        None => ty.to_string(),
                    };
                    args.push(if direction == "out" {
                        format!("->{label}")
                    } else {
                        label
                    });
                }
            }
            ("property", false) => {
                if let Some(i) = iface.as_mut() {
                    let access = match attr(body, "access") {
                        Some("read") => "r",
                        Some("write") => "w",
                        _ => "rw",
                    };
                    i.properties.push(format!(
                        "{}: {} [{access}]",
                        attr(body, "name").unwrap_or("?"),
                        attr(body, "type").unwrap_or("?")
                    ));
                }
            }
            _ => {}
        }
    }
    info
}

fn finish_member(iface: &mut Option<Interface>, (kind, name, args): (&str, String, Vec<String>)) {
    let Some(i) = iface.as_mut() else {
        return;
    };
    let (ins, outs): (Vec<&String>, Vec<&String>) = args.iter().partition(|a| !a.starts_with("->"));
    let ins: Vec<&str> = ins.iter().map(|s| s.as_str()).collect();
    let outs: Vec<&str> = outs.iter().map(|s| s.trim_start_matches("->")).collect();
    let sig = if kind == "signal" {
        format!("{name}({})", outs.join(", "))
    } else if outs.is_empty() {
        format!("{name}({})", ins.join(", "))
    } else {
        format!("{name}({}) -> ({})", ins.join(", "), outs.join(", "))
    };
    if kind == "signal" {
        i.signals.push(sig);
    } else {
        i.methods.push(sig);
    }
}

fn introspect(conn: &Connection, name: &str, path: &str) -> Result<NodeInfo> {
    let reply = conn
        .call_method(
            Some(name),
            path,
            Some("org.freedesktop.DBus.Introspectable"),
            "Introspect",
            &(),
        )
        .map_err(|e| Error::new(format!("Introspect {name} {path} failed: {e}")))?;
    let (xml,): (String,) = reply
        .body()
        .deserialize()
        .map_err(|e| Error::new(format!("Introspect reply: {e}")))?;
    Ok(parse_introspection(&xml))
}

/// Interfaces every object carries; listing them is noise.
fn is_boilerplate(iface: &str) -> bool {
    matches!(
        iface,
        "org.freedesktop.DBus.Introspectable"
            | "org.freedesktop.DBus.Peer"
            | "org.freedesktop.DBus.Properties"
    )
}

fn join_path(parent: &str, child: &str) -> String {
    if parent == "/" {
        format!("/{child}")
    } else {
        format!("{parent}/{child}")
    }
}

/// Paths under `root` that carry non-boilerplate interfaces.
fn walk(conn: &Connection, name: &str, root: &str) -> (Vec<(String, Vec<String>)>, bool) {
    let mut found = Vec::new();
    let mut queue = vec![(root.to_string(), 0usize)];
    let mut visited = 0usize;
    let mut truncated = false;
    while let Some((path, depth)) = queue.pop() {
        visited += 1;
        if visited > WALK_BUDGET {
            truncated = true;
            break;
        }
        let Ok(info) = introspect(conn, name, &path) else {
            continue;
        };
        let ifaces: Vec<String> = info
            .interfaces
            .iter()
            .filter(|i| !is_boilerplate(&i.name))
            .map(|i| i.name.clone())
            .collect();
        if !ifaces.is_empty() {
            found.push((path.clone(), ifaces));
        }
        if depth < WALK_DEPTH {
            for child in info.children.iter().rev() {
                queue.push((join_path(&path, child), depth + 1));
            }
        }
    }
    found.sort();
    (found, truncated)
}

/// The `dbus_inspect` tool, for one bus name.
pub fn inspect(conn: &Connection, name: &str, path: Option<&str>) -> Result<String> {
    let mut out = String::new();
    match path {
        None => {
            let (paths, truncated) = walk(conn, name, "/");
            if paths.is_empty() {
                return Err(Error::with_hint(
                    format!("{name} exposes no interfaces beyond the D-Bus boilerplate"),
                    "check the name with dbus_inspect and no arguments, or the app may \
                     not be introspectable; fall back to the semantic route",
                ));
            }
            let _ = writeln!(out, "{name} — objects with interfaces:");
            for (p, ifaces) in &paths {
                let _ = writeln!(out, "  {p}\n    {}", ifaces.join("\n    "));
            }
            if truncated {
                let _ = writeln!(out, "  [stopped after {WALK_BUDGET} objects]");
            }
            let _ = writeln!(
                out,
                "\nPass `path` to see an object's methods, properties and signals."
            );
        }
        Some(path) => {
            let info = introspect(conn, name, path)?;
            let _ = writeln!(out, "{name} {path}");
            for iface in info.interfaces.iter().filter(|i| !is_boilerplate(&i.name)) {
                let _ = writeln!(out, "\ninterface {}", iface.name);
                for m in &iface.methods {
                    let _ = writeln!(out, "  method   {m}");
                }
                for p in &iface.properties {
                    let _ = writeln!(out, "  property {p}");
                }
                for s in &iface.signals {
                    let _ = writeln!(out, "  signal   {s}");
                }
            }
            if !info.children.is_empty() {
                let _ = writeln!(
                    out,
                    "\nchild objects: {}",
                    info.children
                        .iter()
                        .map(|c| join_path(path, c))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            let _ = writeln!(
                out,
                "\nCall with dbus_call; the `signature` is the concatenated in-arg \
                 types. Read a property with interface \
                 org.freedesktop.DBus.Properties, method Get, signature \"ss\", args \
                 [interface, property]."
            );
        }
    }
    Ok(out)
}

/// The `dbus_inspect` tool with no name: every well-known name on the bus,
/// with the window it belongs to where one can be found.
pub fn list_all(conn: &Connection, windows: &[WindowInfo]) -> Result<String> {
    let mut out = String::from("well-known names on the session bus:\n");
    for name in list_names(conn)? {
        let pid = owner_pid(conn, &name).map(i64::from);
        let owner = windows
            .iter()
            .find(|w| Some(w.pid) == pid)
            .map(|w| format!(" — owned by {} ({})", w.class, w.address))
            .unwrap_or_default();
        let _ = writeln!(out, "  {name}{owner}");
    }
    let _ = writeln!(
        out,
        "\nPass `window` to narrow to one app, or `name` to see what a name exposes."
    );
    Ok(out)
}

// ---------------------------------------------------------------------------
// Calls
// ---------------------------------------------------------------------------

fn json_int(v: &serde_json::Value) -> Result<i128> {
    if let Some(i) = v.as_i64() {
        return Ok(i128::from(i));
    }
    if let Some(u) = v.as_u64() {
        return Ok(i128::from(u));
    }
    // Accept integral numbers written as strings, which JSON callers often
    // use for 64-bit values.
    v.as_str()
        .and_then(|s| s.parse::<i128>().ok())
        .ok_or_else(|| Error::new(format!("expected an integer, got {v}")))
}

fn ranged<T: TryFrom<i128>>(v: &serde_json::Value, ty: &str) -> Result<T> {
    let i = json_int(v)?;
    T::try_from(i).map_err(|_| Error::new(format!("{i} does not fit D-Bus type {ty}")))
}

/// Best guess for a JSON value placed inside a variant (`v`).
fn infer(v: &serde_json::Value) -> Result<Value<'static>> {
    use serde_json::Value as J;
    Ok(match v {
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) if n.is_i64() => match i32::try_from(n.as_i64().unwrap()) {
            Ok(i) => Value::I32(i),
            Err(_) => Value::I64(n.as_i64().unwrap()),
        },
        J::Number(n) if n.is_u64() => Value::U64(n.as_u64().unwrap()),
        J::Number(n) => Value::F64(n.as_f64().unwrap_or(0.0)),
        J::String(s) => Value::from(s.clone()),
        J::Array(items) if items.iter().all(|i| i.is_string()) => {
            to_value(&Signature::array(Signature::Str), v)?
        }
        J::Object(_) => to_value(&Signature::dict(Signature::Str, Signature::Variant), v)?,
        other => {
            return Err(Error::with_hint(
                format!("cannot infer a D-Bus type for {other} inside a variant"),
                "use a string, number, boolean, list of strings, or object",
            ));
        }
    })
}

/// Convert JSON to a D-Bus value of an exact signature.
fn to_value(sig: &Signature, v: &serde_json::Value) -> Result<Value<'static>> {
    let wrong = |what: &str| Error::new(format!("expected {what} for D-Bus type {sig}, got {v}"));
    Ok(match sig {
        Signature::U8 => Value::U8(ranged(v, "y")?),
        Signature::Bool => Value::Bool(v.as_bool().ok_or_else(|| wrong("true/false"))?),
        Signature::I16 => Value::I16(ranged(v, "n")?),
        Signature::U16 => Value::U16(ranged(v, "q")?),
        Signature::I32 => Value::I32(ranged(v, "i")?),
        Signature::U32 => Value::U32(ranged(v, "u")?),
        Signature::I64 => Value::I64(ranged(v, "x")?),
        Signature::U64 => Value::U64(ranged(v, "t")?),
        Signature::F64 => Value::F64(v.as_f64().ok_or_else(|| wrong("a number"))?),
        Signature::Str => Value::from(v.as_str().ok_or_else(|| wrong("a string"))?.to_string()),
        Signature::ObjectPath => {
            let s = v.as_str().ok_or_else(|| wrong("an object path string"))?;
            Value::ObjectPath(
                zvariant::ObjectPath::try_from(s.to_string())
                    .map_err(|e| Error::new(format!("invalid object path {s:?}: {e}")))?,
            )
        }
        Signature::Signature => {
            let s = v.as_str().ok_or_else(|| wrong("a signature string"))?;
            Value::Signature(
                Signature::from_str(s)
                    .map_err(|e| Error::new(format!("invalid signature {s:?}: {e}")))?,
            )
        }
        Signature::Variant => Value::Value(Box::new(infer(v)?)),
        Signature::Array(child) => {
            let items = v.as_array().ok_or_else(|| wrong("a list"))?;
            let mut array = zvariant::Array::new(child.signature());
            for item in items {
                array
                    .append(to_value(child.signature(), item)?)
                    .map_err(|e| Error::new(format!("array element: {e}")))?;
            }
            Value::Array(array)
        }
        Signature::Dict { key, value } => {
            let map = v.as_object().ok_or_else(|| wrong("an object"))?;
            let mut dict = zvariant::Dict::new(key.signature(), value.signature());
            for (k, item) in map {
                // JSON keys are strings; numeric key types parse them.
                let key_value = match key.signature() {
                    Signature::Str | Signature::ObjectPath | Signature::Signature => {
                        to_value(key.signature(), &serde_json::Value::String(k.clone()))?
                    }
                    other => {
                        let n: serde_json::Value = k
                            .parse::<i64>()
                            .map(Into::into)
                            .map_err(|_| Error::new(format!("dict key {k:?} is not a {other}")))?;
                        to_value(other, &n)?
                    }
                };
                dict.append(key_value, to_value(value.signature(), item)?)
                    .map_err(|e| Error::new(format!("dict entry {k:?}: {e}")))?;
            }
            Value::Dict(dict)
        }
        Signature::Structure(fields) => {
            let items = v
                .as_array()
                .ok_or_else(|| wrong("a list of struct fields"))?;
            let fields: Vec<&Signature> = fields.iter().collect();
            if items.len() != fields.len() {
                return Err(wrong(&format!("{} struct fields", fields.len())));
            }
            let mut builder = StructureBuilder::new();
            for (field, item) in fields.iter().zip(items) {
                builder = builder.append_field(to_value(field, item)?);
            }
            Value::Structure(
                builder
                    .build()
                    .map_err(|e| Error::new(format!("struct: {e}")))?,
            )
        }
        other => {
            return Err(Error::with_hint(
                format!("D-Bus type {other} cannot be sent from JSON"),
                "file descriptors and GVariant-only types are not supported here",
            ));
        }
    })
}

/// Build the call body: one value per top-level type in `signature`.
fn build_args(signature: Option<&str>, args: &[serde_json::Value]) -> Result<Vec<Value<'static>>> {
    let signature = signature.unwrap_or("").trim();
    if signature.is_empty() {
        if !args.is_empty() {
            return Err(Error::with_hint(
                "`args` given without a `signature`",
                "pass the method's in-arg types, e.g. \"s\" or \"ss\" — dbus_inspect lists \
                 them for every method",
            ));
        }
        return Ok(Vec::new());
    }
    let parsed = Signature::from_str(&format!("({signature})"))
        .map_err(|e| Error::new(format!("invalid signature {signature:?}: {e}")))?;
    let Signature::Structure(fields) = parsed else {
        unreachable!("a parenthesised signature is a structure");
    };
    let fields: Vec<&Signature> = fields.iter().collect();
    if fields.len() != args.len() {
        return Err(Error::new(format!(
            "signature {signature:?} has {} argument(s) but {} were given",
            fields.len(),
            args.len()
        )));
    }
    fields
        .iter()
        .zip(args)
        .enumerate()
        .map(|(i, (sig, arg))| {
            to_value(sig, arg).map_err(|e| Error::new(format!("argument {}: {}", i + 1, e.message)))
        })
        .collect()
}

/// The `dbus_call` tool.
pub fn call(
    conn: &Connection,
    name: &str,
    path: &str,
    interface: &str,
    method: &str,
    signature: Option<&str>,
    args: &[serde_json::Value],
) -> Result<String> {
    let values = build_args(signature, args)?;
    let reply = if values.is_empty() {
        conn.call_method(Some(name), path, Some(interface), method, &())
    } else {
        let mut builder = StructureBuilder::new();
        for v in values {
            builder = builder.append_field(v);
        }
        let body = builder
            .build()
            .map_err(|e| Error::new(format!("could not build call body: {e}")))?;
        conn.call_method(Some(name), path, Some(interface), method, &body)
    }
    .map_err(|e| {
        Error::with_hint(
            format!("{interface}.{method} on {name} {path} failed: {e}"),
            "check the name, path, interface and signature with dbus_inspect",
        )
    })?;

    let body = reply.body();
    if matches!(body.signature(), Signature::Unit) {
        return Ok(format!("{interface}.{method} returned (no values)"));
    }
    let returned: zvariant::Structure = body
        .deserialize()
        .map_err(|e| Error::new(format!("could not decode the reply: {e}")))?;
    let rendered: Vec<String> = returned.fields().iter().map(|v| v.to_string()).collect();
    Ok(format!(
        "{interface}.{method} returned {}:\n{}",
        body.signature(),
        rendered.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn class_tokens_pick_the_distinctive_part() {
        assert_eq!(
            class_token("org.xfce.mousepad").as_deref(),
            Some("mousepad")
        );
        assert_eq!(
            class_token("io.github.berker_z.Marcel").as_deref(),
            Some("marcel")
        );
        assert_eq!(class_token("kitty").as_deref(), Some("kitty"));
        assert_eq!(class_token("qt"), None);
    }

    #[test]
    fn names_match_whole_components_only() {
        assert!(name_mentions("org.mpris.MediaPlayer2.spotify", "spotify"));
        assert!(name_mentions("org.xfce.mousepad", "mousepad"));
        assert!(name_mentions("org.mpris.MediaPlayer2.vlc.instance2", "vlc"));
        assert!(name_mentions("org.mpris.MediaPlayer2.mpv1234", "mpv"));
        assert!(!name_mentions("org.freedesktop.portal.Desktop", "top"));
        assert!(!name_mentions("org.kittyfans.Foo", "kitty"));
    }

    #[test]
    fn introspection_xml_summarises() {
        let xml = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" "x">
<node>
  <interface name="org.freedesktop.DBus.Peer"><method name="Ping"/></interface>
  <interface name="org.mpris.MediaPlayer2.Player">
    <method name="PlayPause"/>
    <method name="Seek"><arg direction="in" type="x" name="Offset"/></method>
    <method name="Get"><arg type="s" name="key"/><arg direction="out" type="v"/></method>
    <property name="PlaybackStatus" type="s" access="read"/>
    <signal name="Seeked"><arg type="x" name="Position"/></signal>
  </interface>
  <node name="child"/>
  <node name="other"><interface name="x.Y"/></node>
</node>"#;
        let info = parse_introspection(xml);
        assert_eq!(info.children, vec!["child", "other"]);
        let player = info
            .interfaces
            .iter()
            .find(|i| i.name == "org.mpris.MediaPlayer2.Player")
            .unwrap();
        assert_eq!(
            player.methods,
            vec!["PlayPause()", "Seek(Offset: x)", "Get(key: s) -> (v)"]
        );
        assert_eq!(player.properties, vec!["PlaybackStatus: s [r]"]);
        assert_eq!(player.signals, vec!["Seeked(Position: x)"]);
        // The child node's interface is not attributed to the root.
        assert!(!info.interfaces.iter().any(|i| i.name == "x.Y"));
    }

    #[test]
    fn attributes_need_a_word_boundary() {
        assert_eq!(attr(r#"arg typename="q" type="s""#, "type"), Some("s"));
        assert_eq!(attr(r#"method name='Go'"#, "name"), Some("Go"));
    }

    #[test]
    fn json_args_follow_the_signature() {
        let values = build_args(
            Some("sua{sv}ax"),
            &[
                json!("org.mpris.MediaPlayer2.Player"),
                json!(7),
                json!({ "volume": 0.5, "muted": false }),
                json!([1, "-2"]),
            ],
        )
        .unwrap();
        assert_eq!(values.len(), 4);
        assert_eq!(values[1], Value::U32(7));
        assert_eq!(values[2].value_signature().to_string(), "a{sv}");
        assert_eq!(values[3].value_signature().to_string(), "ax");
    }

    #[test]
    fn bad_args_are_explained() {
        assert!(build_args(None, &[json!(1)]).is_err());
        assert!(build_args(Some("s"), &[]).is_err());
        assert!(build_args(Some("u"), &[json!(-1)]).is_err());
        assert!(build_args(Some("y"), &[json!(300)]).is_err());
        assert!(build_args(Some("o"), &[json!("not a path")]).is_err());
        assert!(build_args(Some("(si)"), &[json!(["a", 1])]).is_ok());
        assert!(build_args(Some("(si)"), &[json!(["a"])]).is_err());
    }
}
