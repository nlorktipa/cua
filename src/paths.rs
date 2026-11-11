use std::path::PathBuf;

/// `$XDG_RUNTIME_DIR` (tmpfs, per user), where Wayland sockets also live.
pub fn xdg_runtime() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", rustix::process::getuid().as_raw())))
}

/// Root of everything cua keeps at runtime.
pub fn runtime_dir() -> PathBuf {
    xdg_runtime().join("cua")
}

/// One JSON file per agent; the viewer watches this directory.
pub fn agents_dir() -> PathBuf {
    runtime_dir().join("agents")
}

/// Per-agent scratch space: generated compositor config, PATH shims, logs.
pub fn agent_dir(id: &str) -> PathBuf {
    runtime_dir().join(id)
}

/// Browser profiles for agents (copies of the user's, see `profiles`). Slot N is used by one
/// agent at a time, so agents never share a running browser.
pub fn profile_slot_dir(n: u32) -> PathBuf {
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("cua/profiles").join(format!("slot-{n}"))
}
