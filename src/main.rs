mod a11y;
mod action;
mod capture;
mod compositor;
mod doctor;
mod exec;
mod input;
mod mcp;
mod notes;
mod sh;

const USAGE: &str = "\
hyprhands — computer-use executor for Wayland compositors

USAGE:
    hyprhands mcp        Run as an MCP server over stdio (what agents spawn)
    hyprhands doctor     Check dependencies and report capabilities
    hyprhands --version

The MCP server must run inside your graphical session so it inherits
WAYLAND_DISPLAY, HYPRLAND_INSTANCE_SIGNATURE, and XDG_RUNTIME_DIR.
";

fn main() {
    let mut args = std::env::args().skip(1);
    let arg = args.next();
    let code = match arg.as_deref() {
        Some("mcp") => mcp::serve(),
        Some("doctor") => doctor::run(),
        Some("__fingerprint") => match (args.next(), args.next()) {
            (Some(pid), Some(output)) => notes::fingerprint_helper(&pid, &output),
            _ => 2,
        },
        Some("--version") | Some("-V") => {
            println!("hyprhands {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some("--help") | Some("-h") | None => {
            print!("{USAGE}");
            0
        }
        Some(other) => {
            eprintln!("hyprhands: unknown command {other:?}\n");
            eprint!("{USAGE}");
            2
        }
    };
    std::process::exit(code);
}
