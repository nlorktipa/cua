//! Shared list of running agents: each MCP server writes one small JSON file,
//! the viewer reads them. No daemon needed, and a dead agent is detected by its pid.

use crate::paths;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AgentInfo {
    pub id: String,
    pub pid: u32,
    /// Project the agent works in (basename of its working directory).
    pub name: String,
    /// Short description of what the agent is doing, set via the `set_task` tool.
    #[serde(default)]
    pub task: Option<String>,
    pub cwd: String,
    /// MCP client, e.g. "claude-code 2.1.0".
    pub client: String,
    /// "starting", "ready" or "error".
    pub status: String,
    pub error: Option<String>,
    /// Wayland socket of the agent's virtual screen, e.g. "wayland-2".
    pub wayland_display: Option<String>,
    pub width: u32,
    pub height: u32,
    pub started_at: u64,
    pub actions: u64,
    pub last_action: Option<String>,
    /// Unix time in milliseconds.
    pub last_action_at: Option<u64>,
    /// Screen point the last pointer action targeted.
    pub last_point: Option<(i32, i32)>,
    /// What the user wants from this agent; set by the viewer, read from its own file.
    #[serde(skip)]
    pub control: Control,
}

/// The user's say over an agent, set from the viewer and obeyed by the agent's MCP server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Control {
    /// The agent uses its screen.
    #[default]
    Run,
    /// The agent's tool calls are refused until the user resumes it.
    Stop,
    /// The agent is stopped and the user drives its screen from the viewer.
    Takeover,
    /// The user removed the agent: its screen closes and it leaves the sidebar.
    Close,
}

fn control_path(id: &str) -> std::path::PathBuf {
    paths::agents_dir().join(format!("{id}.control"))
}

pub fn control(id: &str) -> Control {
    match std::fs::read_to_string(control_path(id)).as_deref().map(str::trim) {
        Ok("stop") => Control::Stop,
        Ok("takeover") => Control::Takeover,
        Ok("close") => Control::Close,
        _ => Control::Run,
    }
}

pub fn set_control(id: &str, control: Control) -> anyhow::Result<()> {
    let word = match control {
        Control::Run => {
            match std::fs::remove_file(control_path(id)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => return Ok(()),
            }
        }
        Control::Stop => "stop",
        Control::Takeover => "takeover",
        Control::Close => "close",
    };
    std::fs::create_dir_all(paths::agents_dir())?;
    std::fs::write(control_path(id), word)?;
    Ok(())
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn pid_alive(pid: u32) -> bool {
    // Signal 0 only checks that the process exists and is ours.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

pub fn write(info: &AgentInfo) -> anyhow::Result<()> {
    let dir = paths::agents_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", info.id));
    let tmp = dir.join(format!(".{}.json.tmp", info.id));
    std::fs::write(&tmp, serde_json::to_vec_pretty(info)?)?;
    std::fs::rename(tmp, path)?; // atomic: readers never see half a file
    Ok(())
}

/// Forget an agent entirely: its entry and the user's control over it.
pub fn remove(id: &str) {
    unlist(id);
    let _ = std::fs::remove_file(control_path(id));
}

/// Take an agent off the list, keeping the user's control over it.
pub fn unlist(id: &str) {
    let _ = std::fs::remove_file(paths::agents_dir().join(format!("{id}.json")));
}

/// Live agents the user has not removed, oldest first. Entries left behind by crashed agents are cleaned up.
pub fn list() -> Vec<AgentInfo> {
    let Ok(entries) = std::fs::read_dir(paths::agents_dir()) else {
        return Vec::new();
    };
    let mut agents: Vec<AgentInfo> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
        .filter_map(|e| {
            let mut info: AgentInfo = serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()?;
            if !pid_alive(info.pid) {
                remove(&info.id);
                return None;
            }
            info.control = control(&info.id);
            (info.control != Control::Close).then_some(info)
        })
        .collect();
    agents.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.id.cmp(&b.id)));
    agents
}

/// Clean up after agents that died without cleaning up (e.g. killed by their
/// client): stop leftover processes from their screens and remove their files.
pub fn gc() {
    let Ok(entries) = std::fs::read_dir(paths::runtime_dir()) else { return };
    for entry in entries.flatten() {
        let id = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = id.rsplit('-').next().and_then(|p| p.parse::<u32>().ok()) else { continue };
        if pid_alive(pid) {
            continue;
        }
        let needle = format!("CUA_AGENT={id}");
        for proc in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Ok(p) = proc.file_name().to_string_lossy().parse::<i32>() else { continue };
            let Ok(env) = std::fs::read(proc.path().join("environ")) else { continue };
            if env.split(|b| *b == 0).any(|kv| kv == needle.as_bytes()) {
                unsafe { libc::kill(p, libc::SIGKILL) };
            }
        }
        let _ = std::fs::remove_dir_all(entry.path());
        remove(&id);
    }
}
