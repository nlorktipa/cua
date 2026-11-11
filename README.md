<p align="center">
  <img src="assets/logo.svg" width="128" height="128" alt="cua logo">
</p>

<h1 align="center">cua</h1>

Computer use for Claude Code on Linux with Hyprland: every Claude Code session gets its own virtual screen on your computer, with the same apps, files, wallpaper, bar and dock. Agents work in parallel without touching your mouse, your keyboard or each other.

https://github.com/user-attachments/assets/0fe0c0dc-5a1e-4ab2-9ffb-8eea6ae3515c

> **Limitation:** cua currently works only on Hyprland, because each agent's screen is a copy of your own desktop; other desktops (GNOME, KDE, sway, X11…) each need their own backend, and contributions adding them are very welcome.

## How it works

```mermaid
flowchart LR
    V["cua (viewer)"] -- "on start: claude mcp add cua" --> CC
    subgraph CC["Claude Code sessions"]
        A1["Agent 1"]
        A2["Agent 2"]
    end
    A1 -- "MCP stdio" --> M1["cua mcp"]
    A2 -- "MCP stdio" --> M2["cua mcp"]
    M1 -- "screenshot · click · type · keys" --> S1["Virtual screen 1<br/>nested Hyprland, headless output"]
    M2 -- "screenshot · click · type · keys" --> S2["Virtual screen 2<br/>nested Hyprland, headless output"]
    S1 -. "live frames" .-> V
    S2 -. "live frames" .-> V
    S1 --- D["Your desktop: same apps, files,<br/>bar, dock and wallpaper"]
    S2 --- D
```

## Build and run

Needs Rust, and Hyprland installed and running as your Wayland compositor (each agent's screen is a nested Hyprland instance).

```sh
cargo build --release
./target/release/cua
```

On start, `cua` adds itself to your app launcher (`~/.local/share/applications/cua.desktop`) and registers itself with Claude Code as the `cua` MCP server (run `/mcp` in sessions that were already open). Ask Claude Code to do something on the computer. Each agent shows up on the left; click one to watch its screen live.

Agents act as you: their apps run as your user, with your files, SSH agent, keyring and permissions. Browsers and web apps on an agent's screen start from a fresh copy of your browser profile (Chromium, Chrome, Brave, Edge, Vivaldi, Helium, Firefox, LibreWolf, Zen, Floorp), so they're logged in to the same sites with the same extensions. The copy is refreshed each time the agent opens its browser, and nothing the agent does there writes back to your profile.

