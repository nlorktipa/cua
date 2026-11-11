mod config;
mod display;
mod gui;
mod hypr;
mod keys;
mod mcp;
mod paths;
mod registry;
mod setup;
mod wl;

const USAGE: &str = "cua — computer use for Claude Code on its own virtual screens

Usage:
  cua           Open the viewer (and register the MCP server with Claude Code)
  cua mcp       Run the MCP server (Claude Code starts this itself)
  cua setup     Only register the MCP server with Claude Code and install the app launcher entry
  cua list      List running agents";

fn main() -> anyhow::Result<()> {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        None | Some("gui") => gui::run(),
        Some("mcp") => mcp::run(),
        Some("setup") => {
            setup::install_desktop_entry()?;
            match setup::ensure_registered()? {
                setup::Setup::Ready => println!("cua is already registered with Claude Code."),
                setup::Setup::Registered => {
                    println!("Registered cua with Claude Code. Run /mcp in open sessions (or restart them).")
                }
            }
            Ok(())
        }
        Some("list") => {
            registry::gc();
            for a in registry::list() {
                println!(
                    "{:<28} {:<8} {:<11} {}",
                    a.id,
                    a.status,
                    a.wayland_display.as_deref().unwrap_or("-"),
                    a.last_action.as_deref().unwrap_or("")
                );
            }
            Ok(())
        }
        Some("-V" | "--version") => {
            println!("cua {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown command {other:?}\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}
