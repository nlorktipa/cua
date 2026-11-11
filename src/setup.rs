//! Make sure Claude Code knows about cua: register this binary as a user-scope
//! MCP server named "cua", or repoint an existing registration at this binary.

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub enum Setup {
    /// Already registered with this binary.
    Ready,
    /// Newly registered (or updated); running sessions need `/mcp` or a restart.
    Registered,
}

fn claude_json() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir).join(".claude.json"),
        None => dirs::home_dir().unwrap_or_default().join(".claude.json"),
    }
}

fn exe() -> Result<String> {
    let exe = std::env::current_exe()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    Ok(exe.to_string_lossy().into_owned())
}

fn current_entry() -> Option<Value> {
    let text = std::fs::read_to_string(claude_json()).ok()?;
    let config: Value = serde_json::from_str(&text).ok()?;
    config.get("mcpServers")?.get("cua").cloned()
}

pub fn ensure_registered() -> Result<Setup> {
    let exe = exe()?;
    let entry = current_entry();
    if let Some(e) = &entry {
        let same_cmd = e.get("command").and_then(Value::as_str) == Some(exe.as_str());
        let same_args = e.get("args") == Some(&json!(["mcp"]));
        if same_cmd && same_args {
            return Ok(Setup::Ready);
        }
    }

    // Prefer the CLI: it owns the file format and handles concurrent writers.
    if crate::display::which("claude").is_some() {
        if entry.is_some() {
            let _ = Command::new("claude").args(["mcp", "remove", "--scope", "user", "cua"]).output();
        }
        let out = Command::new("claude")
            .args(["mcp", "add", "--scope", "user", "cua", "--", &exe, "mcp"])
            .output()
            .context("running `claude mcp add`")?;
        if out.status.success() {
            return Ok(Setup::Registered);
        }
        eprintln!("cua: `claude mcp add` failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }

    // Fallback: edit ~/.claude.json directly, atomically.
    let path = claude_json();
    let mut config: Value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        Err(_) => json!({}),
    };
    let obj = config.as_object_mut().ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))?;
    let servers = obj.entry("mcpServers").or_insert_with(|| json!({}));
    servers
        .as_object_mut()
        .ok_or_else(|| anyhow!("mcpServers is not an object"))?
        .insert("cua".into(), json!({ "type": "stdio", "command": exe, "args": ["mcp"], "env": {} }));
    let tmp = path.with_extension("json.cua-tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&config)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(Setup::Registered)
}
