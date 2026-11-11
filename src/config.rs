//! User configuration, read from `~/.config/cua/config.toml`. Every key is optional.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Size of each agent's virtual screen. 1280x800 keeps screenshots at a
    /// resolution Claude reads well, so coordinates map 1:1.
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
    /// Window animations on the agent screens. Off by default so screenshots
    /// never catch a window mid-animation.
    pub animations: bool,
    /// Commands that draw the desktop shell (bar, dock, wallpaper) on each agent
    /// screen. Empty means: mirror whatever shell is running on your desktop.
    pub shell: Vec<String>,
    /// How long to let the screen settle before the screenshot that follows an action.
    pub settle_ms: u64,
    /// Delay between synthetic key events.
    pub key_delay_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 800,
            refresh: 30,
            animations: false,
            shell: Vec::new(),
            settle_ms: 500,
            key_delay_ms: 4,
        }
    }
}

pub fn load() -> Config {
    let Some(path) = dirs::config_dir().map(|d| d.join("cua/config.toml")) else {
        return Config::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
            eprintln!("cua: ignoring {}: {e}", path.display());
            Config::default()
        }),
        Err(_) => Config::default(),
    }
}
