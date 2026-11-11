//! An agent's virtual screen: a nested Hyprland drawing into a headless output,
//! running the same apps, wallpaper, bar and dock as the user's desktop, with
//! its own cursor, keyboard focus and clipboard.

use crate::config::Config;
use crate::hypr::{self, Instance};
use crate::paths;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A process started on the agent's screen.
pub struct Launched {
    pub pid: u32,
    exit: Arc<Mutex<Option<std::process::ExitStatus>>>,
    log: PathBuf,
}

impl Launched {
    /// If it already failed: its exit status and last output lines.
    pub fn failure(&self) -> Option<String> {
        let status = (*self.exit.lock().unwrap())?;
        if status.success() {
            return None;
        }
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        Some(format!("{status}: {}", lines[lines.len().saturating_sub(8)..].join("\n")))
    }
}

pub struct Display {
    pub instance: Instance,
    lua: bool,
    has_shell: bool,
    launches: u32,
    /// Held while running: claims this agent's browser profile slot.
    _profile_lock: std::fs::File,
    child: Child,
    dir: PathBuf,
    /// Environment of the nested session (WAYLAND_DISPLAY, DISPLAY, ...).
    env: Vec<(OsString, OsString)>,
}

impl Display {
    pub fn start(id: &str, cfg: &Config) -> Result<Display> {
        which("Hyprland").ok_or_else(|| anyhow!("Hyprland is not installed; cua needs it for agent screens"))?;
        let host = hypr::host_instance();
        let host_display = std::env::var("WAYLAND_DISPLAY")
            .ok()
            .filter(|d| paths::xdg_runtime().join(d).exists())
            .or_else(|| host.as_ref().map(|h| h.wayland_display.clone()))
            .ok_or_else(|| anyhow!("no Wayland session found to attach agent screens to"))?;

        let dir = paths::agent_dir(id);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let shim_dir = dir.join("bin");
        let (profiles, profile_lock) = acquire_profile_slot()?;
        write_shims(&shim_dir, &profiles)?;

        if let Some(host) = &host {
            hypr::hide_nested_windows_on(host);
        }
        let (config, lua) = hypr::write_nested_config(&dir, cfg)?;
        let log = std::fs::File::create(dir.join("hyprland.log"))?;

        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut new_path = shim_dir.clone().into_os_string();
        new_path.push(":");
        new_path.push(path);

        let mut cmd = Command::new("Hyprland");
        cmd.arg("-c")
            .arg(&config)
            .env("WAYLAND_DISPLAY", &host_display)
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .env_remove("DISPLAY")
            // Never touch the user's systemd/D-Bus activation environment.
            .env("HYPRLAND_NO_SD_VARS", "1")
            .env("HYPRLAND_NO_SD_NOTIFY", "1")
            .env("HYPRLAND_NO_CRASHREPORTER", "1")
            // Keep file pickers inside the agent's screen instead of the session portal.
            .env("GDK_DEBUG", "no-portals")
            .env("GTK_USE_PORTAL", "0")
            .env("PATH", new_path)
            .env("CUA_AGENT", id)
            .current_dir(dirs::home_dir().unwrap_or_else(|| "/".into()))
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                // Die with the agent, even if it is killed outright.
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = cmd.spawn().context("starting Hyprland")?;
        let pid = child.id();

        let instance = wait_for(Duration::from_secs(15), || {
            if let Ok(Some(status)) = child.try_wait() {
                return Some(Err(anyhow!("Hyprland exited ({status}): {}", log_tail(&dir))));
            }
            hypr::instance_by_pid(pid).map(Ok)
        })
        .ok_or_else(|| anyhow!("Hyprland did not come up: {}", log_tail(&dir)))??;

        let mut display = Display { instance, lua, has_shell: false, launches: 0, _profile_lock: profile_lock, child, dir, env: Vec::new() };
        display.setup_output().inspect_err(|_| display.stop())?;
        display.env = display.capture_env();
        display.has_shell = display.start_shell(cfg, &host_display);
        Ok(display)
    }

    /// Swap the window Hyprland opened on the host for a headless output.
    fn setup_output(&mut self) -> Result<()> {
        let sig = self.instance.signature.clone();
        let monitors = |sig: &str| -> Vec<String> {
            hypr::ipc_json(sig, "monitors all")
                .ok()
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|m| m["name"].as_str().map(str::to_string))
                .collect()
        };
        if !monitors(&sig).iter().any(|m| m == hypr::OUTPUT) {
            hypr::ipc(&sig, &format!("/output create headless {}", hypr::OUTPUT))?;
        }
        wait_for(Duration::from_secs(5), || monitors(&sig).iter().any(|m| m == hypr::OUTPUT).then_some(()))
            .ok_or_else(|| anyhow!("headless output did not appear"))?;
        for name in monitors(&sig).into_iter().filter(|m| m != hypr::OUTPUT) {
            let _ = hypr::ipc(&sig, &format!("/output remove {name}"));
        }
        Ok(())
    }

    /// The environment Hyprland gives its children, so apps cua starts behave
    /// exactly like apps started from the agent's desktop.
    fn capture_env(&self) -> Vec<(OsString, OsString)> {
        let file = self.dir.join("env");
        let cmd = format!("env -0 > '{}.tmp' && mv '{0}.tmp' '{0}'", file.display());
        if hypr::exec(&self.instance.signature, self.lua, &cmd).is_ok()
            && let Some(bytes) = wait_for(Duration::from_secs(3), || std::fs::read(&file).ok()) {
                let env: Vec<_> = bytes
                    .split(|b| *b == 0)
                    .filter_map(|kv| {
                        let i = kv.iter().position(|b| *b == b'=')?;
                        Some((OsString::from_vec(kv[..i].to_vec()), OsString::from_vec(kv[i + 1..].to_vec())))
                    })
                    .collect();
                if env.iter().any(|(k, _)| k == "WAYLAND_DISPLAY") {
                    return env;
                }
            }
        // Fallback: our own environment pointed at the nested session.
        let mut env: Vec<_> = std::env::vars_os()
            .filter(|(k, _)| k != "DISPLAY" && k != "WAYLAND_DISPLAY" && k != "HYPRLAND_INSTANCE_SIGNATURE")
            .collect();
        env.push(("WAYLAND_DISPLAY".into(), self.instance.wayland_display.clone().into()));
        env.push(("HYPRLAND_INSTANCE_SIGNATURE".into(), self.instance.signature.clone().into()));
        env
    }

    /// Returns whether anything was started.
    fn start_shell(&self, cfg: &Config, host_display: &str) -> bool {
        let shell: Vec<Spawn> = if cfg.shell.is_empty() {
            mirror_shell(host_display)
        } else {
            cfg.shell.iter().map(|c| (vec!["sh".into(), "-c".into(), c.into()], Vec::new())).collect()
        };
        let mut started = false;
        for (argv, extra_env) in shell {
            match self.spawn(&argv, &extra_env, "shell.log") {
                Ok(_) => started = true,
                Err(e) => eprintln!("cua: could not start {:?}: {e}", argv),
            }
        }
        started
    }

    fn spawn(&self, argv: &[OsString], extra_env: &[(OsString, OsString)], log: &str) -> Result<Launched> {
        let log_path = self.dir.join(log);
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .envs(extra_env.iter().map(|(k, v)| (k, v)))
            .current_dir(dirs::home_dir().unwrap_or_else(|| "/".into()))
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        let pid = child.id();
        let exit = Arc::new(Mutex::new(None));
        let status = exit.clone();
        // Reap it whenever it exits so it never lingers as a zombie.
        std::thread::spawn(move || {
            let code = child.wait().ok();
            *status.lock().unwrap() = code;
        });
        Ok(Launched { pid, exit, log: log_path })
    }

    /// Start a shell command on the agent's screen.
    pub fn launch(&mut self, command: &str) -> Result<Launched> {
        self.launches += 1;
        let log = format!("launch-{}.log", self.launches);
        self.spawn(&["sh".into(), "-c".into(), command.into()], &[], &log)
    }

    /// Wait until the shell (bar, wallpaper...) has put its surfaces on screen.
    /// Returns false if no shell was started.
    pub fn wait_for_shell(&self, timeout: Duration) -> bool {
        if !self.has_shell {
            return false;
        }
        wait_for(timeout, || {
            let layers = hypr::ipc(&self.instance.signature, "j/layers").ok()?;
            layers.contains("\"namespace\"").then_some(())
        });
        true
    }

    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn ipc_json(&self, command: &str) -> Result<serde_json::Value> {
        hypr::ipc_json(&self.instance.signature, command)
    }

    /// Close every app on this screen, then the compositor itself.
    pub fn stop(&mut self) {
        let sig = self.instance.signature.clone();
        let needle = format!("HYPRLAND_INSTANCE_SIGNATURE={sig}");
        let compositor = self.child.id();
        let me = std::process::id();
        let apps = || -> Vec<u32> {
            all_pids()
                .into_iter()
                .filter(|p| *p != compositor && *p != me)
                .filter(|p| {
                    std::fs::read(format!("/proc/{p}/environ"))
                        .is_ok_and(|env| env.split(|b| *b == 0).any(|kv| kv == needle.as_bytes()))
                })
                .collect()
        };
        // Let apps exit cleanly first (browsers flush their profile), then the compositor.
        let signal = |pids: &[u32], sig: i32| pids.iter().for_each(|p| unsafe {
            libc::kill(*p as i32, sig);
        });
        // Clients may kill us soon after asking us to stop, so keep this short,
        // and drop our files first (running processes keep what they opened).
        let _ = std::fs::remove_dir_all(&self.dir);
        signal(&apps(), libc::SIGTERM);
        wait_for(Duration::from_millis(1500), || apps().is_empty().then_some(()));
        signal(&apps(), libc::SIGKILL);
        unsafe { libc::kill(compositor as i32, libc::SIGTERM) };
        wait_for(Duration::from_secs(1), || (!matches!(self.child.try_wait(), Ok(None))).then_some(()));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Claim the first browser profile slot no other agent holds.
fn acquire_profile_slot() -> Result<(PathBuf, std::fs::File)> {
    for n in 1..=64 {
        let dir = paths::profile_slot_dir(n);
        std::fs::create_dir_all(&dir)?;
        let lock = std::fs::File::create(dir.join(".lock"))?;
        if rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok() {
            return Ok((dir, lock));
        }
    }
    bail!("no free browser profile slot")
}

fn wait_for<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

fn log_tail(dir: &Path) -> String {
    let text = std::fs::read_to_string(dir.join("hyprland.log")).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(5)..].join(" | ")
}

fn all_pids() -> Vec<u32> {
    std::fs::read_dir("/proc")
        .map(|d| d.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect())
        .unwrap_or_default()
}

pub fn which(name: &str) -> Option<PathBuf> {
    which_except(name, None)
}

fn which_except(name: &str, skip: Option<&Path>) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|d| Some(d.as_path()) != skip)
        .map(|d| d.join(name))
        .find(|p| p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false))
}

/// A command line plus extra environment to start it with.
type Spawn = (Vec<OsString>, Vec<(OsString, OsString)>);

/// Programs that draw a desktop shell: bars, docks, wallpapers.
const SHELL_PROGRAMS: &[&str] = &[
    "quickshell", "qs", "waybar", "swaybg", "hyprpaper", "swww-daemon", "mpvpaper", "wpaperd", "wbg",
    "nwg-dock-hyprland", "nwg-dock", "nwg-panel", "hyprpanel", "ironbar", "yambar", "sfwbar", "gBar",
    "ashell", "eww",
];

/// Variables that tie a process to the host session or its supervisor.
const HOST_ONLY_ENV: &[&str] = &[
    "WAYLAND_DISPLAY", "DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE", "PATH", "INVOCATION_ID", "JOURNAL_STREAM",
    "SYSTEMD_EXEC_PID", "MANAGERPID", "MANAGERPIDFDID", "NOTIFY_SOCKET", "LISTEN_PID", "LISTEN_FDS",
    "LISTEN_FDNAMES", "WATCHDOG_PID", "WATCHDOG_USEC", "XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID",
    "SHLVL", "_", "PWD", "OLDPWD",
];

/// Find the shell processes running on the user's desktop and return how to
/// start the same ones (same arguments, same environment) on an agent screen.
fn mirror_shell(host_display: &str) -> Vec<Spawn> {
    let me = rustix::process::getuid().as_raw();
    let mut seen = HashMap::new();
    for pid in all_pids() {
        let Ok(meta) = std::fs::metadata(format!("/proc/{pid}")) else { continue };
        if std::os::unix::fs::MetadataExt::uid(&meta) != me {
            continue;
        }
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else { continue };
        let mut argv: Vec<OsString> = cmdline
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| OsString::from_vec(a.to_vec()))
            .collect();
        let base = |a: &OsString| Path::new(a).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let Some(first) = argv.first() else { continue };
        let mut program = base(first);
        if program.starts_with("python") || program == "gjs" || program == "node" {
            program = argv.get(1).map(base).unwrap_or_default();
        }
        if !SHELL_PROGRAMS.contains(&program.as_str()) {
            continue;
        }
        let Ok(environ) = std::fs::read(format!("/proc/{pid}/environ")) else { continue };
        let env: Vec<(OsString, OsString)> = environ
            .split(|b| *b == 0)
            .filter_map(|kv| {
                let i = kv.iter().position(|b| *b == b'=')?;
                Some((OsString::from_vec(kv[..i].to_vec()), OsString::from_vec(kv[i + 1..].to_vec())))
            })
            .collect();
        let on_host = env.iter().any(|(k, v)| k == "WAYLAND_DISPLAY" && v == host_display);
        if !on_host {
            continue; // another agent's shell, or unrelated
        }
        if program == "quickshell" || program == "qs" {
            // "--no-duplicate" would make the copy exit because the user's shell is running.
            argv.retain(|a| a != "-n" && a != "--no-duplicate" && a != "-d" && a != "--daemonize");
        }
        let env = env.into_iter().filter(|(k, _)| !HOST_ONLY_ENV.iter().any(|h| k == h)).collect();
        seen.entry(argv.clone()).or_insert((argv, env));
    }
    seen.into_values().collect()
}

/// Wrappers placed first on the agent's PATH:
/// - `uwsm-app`/`uwsm app` normally hand apps to the session's app daemon,
///   which would open them on the user's screen. Here they start in place.
/// - Browsers get cua's own profile; otherwise they would attach to the
///   user's running browser and open the window there.
fn write_shims(dir: &Path, profiles: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let write = |name: &str, body: String| -> Result<()> {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n# cua: keeps this launch on the agent's screen.\n{body}"))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        Ok(())
    };

    write(
        "uwsm-app",
        r#"term=0
while [ $# -gt 0 ]; do
  case "$1" in
    --) shift; break ;;
    -T) term=1; shift ;;
    -s|-t|-a|-u|-d) shift 2 ;;
    -*) shift ;;
    *) break ;;
  esac
done
[ $# -eq 0 ] && exit 0
[ $term = 1 ] && set -- xdg-terminal-exec "$@"
setsid -f "$@" >/dev/null 2>&1 </dev/null
"#
        .into(),
    )?;
    if let Some(uwsm) = which_except("uwsm", Some(dir)) {
        write(
            "uwsm",
            format!(
                "if [ \"$1\" = app ]; then shift; exec \"$(dirname \"$0\")/uwsm-app\" \"$@\"; fi\nexec '{}' \"$@\"\n",
                uwsm.display()
            ),
        )?;
    }

    for browser in [
        "chromium", "google-chrome", "google-chrome-stable", "google-chrome-beta", "brave", "brave-browser",
        "microsoft-edge", "microsoft-edge-stable", "vivaldi", "vivaldi-stable", "helium-browser",
    ] {
        if let Some(real) = which_except(browser, Some(dir)) {
            let profile = profiles.join(browser);
            write(
                browser,
                format!(
                    "case \" $* \" in *\" --user-data-dir\"*) exec '{real}' \"$@\" ;; esac\nexec '{real}' --user-data-dir='{profile}' --no-first-run --no-default-browser-check \"$@\"\n",
                    real = real.display(),
                    profile = profile.display()
                ),
            )?;
        }
    }
    for browser in ["firefox", "librewolf", "zen-browser", "floorp"] {
        if let Some(real) = which_except(browser, Some(dir)) {
            let profile = profiles.join(browser);
            write(
                browser,
                format!(
                    "case \" $* \" in *\" --profile \"*|*\" -P \"*) exec '{real}' \"$@\" ;; esac\nmkdir -p '{profile}'\nexec '{real}' --no-remote --profile '{profile}' \"$@\"\n",
                    real = real.display(),
                    profile = profile.display()
                ),
            )?;
        }
    }
    if which_except("Hyprland", Some(dir)).is_none() {
        bail!("Hyprland not found on PATH");
    }
    Ok(())
}
