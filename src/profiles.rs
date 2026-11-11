//! Agents' browsers start from a copy of the user's own browser profile, so
//! they are logged in to the same sites with the same extensions and settings.
//!
//! The user's profile itself can't be shared: a browser allows one running
//! instance per profile, and a second launch hands its window to the first
//! (which would open it on the user's screen). So each agent gets a snapshot,
//! refreshed every time its browser starts. What the agent does there stays in
//! the copy and never touches the user's profile.

use std::path::{Path, PathBuf};

/// Chromium-family browsers (shim name -> user data dir under ~/.config).
pub const CHROMIUM: &[(&str, &str)] = &[
    ("chromium", "chromium"),
    ("google-chrome", "google-chrome"),
    ("google-chrome-stable", "google-chrome"),
    ("google-chrome-beta", "google-chrome-beta"),
    ("brave", "BraveSoftware/Brave-Browser"),
    ("brave-browser", "BraveSoftware/Brave-Browser"),
    ("microsoft-edge", "microsoft-edge"),
    ("microsoft-edge-stable", "microsoft-edge"),
    ("vivaldi", "vivaldi"),
    ("vivaldi-stable", "vivaldi"),
    ("helium-browser", "net.imput.helium"),
];

/// Firefox-family browsers (shim name -> directories holding profiles.ini, relative to home).
pub const FIREFOX: &[(&str, &[&str])] = &[
    ("firefox", &[".config/mozilla/firefox", ".mozilla/firefox"]),
    ("librewolf", &[".config/librewolf/librewolf", ".librewolf"]),
    ("zen-browser", &[".config/zen", ".zen"]),
    ("floorp", &[".config/floorp", ".floorp"]),
];

/// Left out of the copy: caches (rebuilt on demand), crash dumps, the
/// running browser's locks, and the open tabs (the agent starts on a clean window).
const SKIP: &[&str] = &[
    // Chromium
    "Cache", "Code Cache", "GPUCache", "GPUPersistentCache", "DawnGraphiteCache", "DawnWebGPUCache",
    "GraphiteDawnCache", "GrShaderCache", "ShaderCache", "component_crx_cache", "extensions_crx_cache",
    "Crash Reports", "Crashpad", "BrowserMetrics", "SingletonLock", "SingletonSocket", "SingletonCookie",
    "lockfile", "Sessions", "Current Session", "Current Tabs", "Last Session", "Last Tabs",
    // Firefox
    "lock", ".parentlock", "parent.lock", "cache2", "startupCache", "crashes", "minidumps",
    "sessionstore.jsonlz4", "sessionstore-backups",
];

/// The user's profile directory for `browser`, if they have one.
pub fn user_profile(browser: &str) -> Option<PathBuf> {
    if let Some((_, dir)) = CHROMIUM.iter().find(|(b, _)| *b == browser) {
        return Some(dirs::config_dir()?.join(dir)).filter(|p| p.is_dir());
    }
    let (_, roots) = FIREFOX.iter().find(|(b, _)| *b == browser)?;
    let home = dirs::home_dir()?;
    roots.iter().map(|r| home.join(r)).find_map(|root| firefox_default_profile(&root))
}

/// The profile Firefox opens by default, from `profiles.ini`.
fn firefox_default_profile(root: &Path) -> Option<PathBuf> {
    let ini = std::fs::read_to_string(root.join("profiles.ini")).ok()?;
    let mut sections: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for line in ini.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            sections.push((name.to_string(), Vec::new()));
        } else if let (Some((k, v)), Some(s)) = (line.split_once('='), sections.last_mut()) {
            s.1.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let get = |kv: &[(String, String)], key: &str| kv.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    let resolve = |path: String, relative: bool| if relative { root.join(path) } else { PathBuf::from(path) };
    // [Install…] Default= is what current Firefox uses; [Profile…] Default=1 is the legacy marker.
    let install = sections
        .iter()
        .filter(|(n, _)| n.starts_with("Install"))
        .find_map(|(_, kv)| get(kv, "Default"))
        .map(|p| resolve(p, true));
    let legacy = || {
        let profiles: Vec<_> = sections.iter().filter(|(n, _)| n.starts_with("Profile")).collect();
        let pick = profiles
            .iter()
            .find(|(_, kv)| get(kv, "Default").as_deref() == Some("1"))
            .or_else(|| profiles.first())?;
        let path = get(&pick.1, "Path")?;
        Some(resolve(path, get(&pick.1, "IsRelative").as_deref() != Some("0")))
    };
    install.or_else(legacy).filter(|p| p.is_dir())
}

/// Replace `dest` with a fresh copy of the user's profile for `browser`,
/// unless a browser is running on `dest` right now (then it is left alone, and
/// the launch opens a window in that browser).
pub fn sync(browser: &str, dest: &Path) -> anyhow::Result<()> {
    let Some(src) = user_profile(browser) else { return Ok(()) };
    if in_use(dest) {
        return Ok(());
    }
    let parent = dest.parent().ok_or_else(|| anyhow::anyhow!("bad profile path"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".{browser}.new"));
    let _ = std::fs::remove_dir_all(&tmp);
    copy_tree(&src, &tmp);
    let _ = std::fs::remove_dir_all(dest);
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

/// Whether some process has `dir` on its command line (a browser using it).
fn in_use(dir: &Path) -> bool {
    let needle = dir.as_os_str().as_encoded_bytes();
    let me = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else { return false };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| *pid != me)
        .filter_map(|pid| std::fs::read(format!("/proc/{pid}/cmdline")).ok())
        // Not the shim's own `sh` (which names the dir too): only the browser.
        .filter(|cmd| !cmd.starts_with(b"/bin/sh\0") && !cmd.starts_with(b"sh\0"))
        .any(|cmd| cmd.windows(needle.len()).any(|w| w == needle))
}

/// Copy a directory tree, skipping caches and locks. Best effort: files that
/// vanish or can't be read while the user's browser runs are left out. On
/// btrfs/XFS `std::fs::copy` shares the data blocks, so this is near-instant.
fn copy_tree(src: &Path, dest: &Path) {
    if std::fs::create_dir_all(dest).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(src) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if SKIP.iter().any(|s| name == *s) {
            continue;
        }
        let (from, to) = (entry.path(), dest.join(&name));
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            copy_tree(&from, &to);
        } else if kind.is_symlink() {
            if let Ok(target) = std::fs::read_link(&from) {
                let _ = std::os::unix::fs::symlink(target, &to);
            }
        } else if kind.is_file() {
            let _ = std::fs::copy(&from, &to);
        }
    }
}
