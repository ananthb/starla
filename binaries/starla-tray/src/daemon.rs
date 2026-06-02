//! Start / stop / restart the probe daemon from the tray.
//!
//! The tray and daemon are separate processes. The user normally starts
//! the daemon via launchd (macOS) or systemd --user (Linux). These
//! controls drive the same service manager so the daemon's autostart and
//! restart policy are preserved — the tray never owns the daemon process
//! directly.

use std::process::{Command, Stdio};

/// Whether daemon controls are wired up on this platform.
#[allow(dead_code)]
pub const SUPPORTED: bool = cfg!(any(target_os = "macos", target_os = "linux"));

/// macOS LaunchAgent label and plist filename used by both the Homebrew
/// cask postflight and the Nix home-manager module.
#[cfg(target_os = "macos")]
const LAUNCHD_LABEL: &str = "com.ananthb.starla";

/// systemd unit name on Linux (user or system scope).
#[cfg(target_os = "linux")]
const SYSTEMD_UNIT: &str = "starla";

/// Start (or load) the probe daemon.
pub fn start() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // If the agent is already bootstrapped, `kickstart` (re)launches
        // it. Otherwise we need `bootstrap <plist>` to load it first.
        let target = launchd_target();
        if run_launchctl(&["kickstart", &target]).is_ok() {
            return Ok(());
        }
        if let Some(plist) = plist_path() {
            run_launchctl(&["bootstrap", &launchd_domain(), &plist.to_string_lossy()])?;
            return Ok(());
        }
        anyhow::bail!("no LaunchAgent plist found at ~/Library/LaunchAgents/{LAUNCHD_LABEL}.plist");
    }
    #[cfg(target_os = "linux")]
    {
        run_systemctl(&["start", SYSTEMD_UNIT])
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        anyhow::bail!("daemon controls are not supported on this platform")
    }
}

/// Stop the probe daemon. On macOS this `bootout`s the agent so launchd
/// will not respawn it via `KeepAlive` until the user starts it again.
pub fn stop() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        run_launchctl(&["bootout", &launchd_target()])
    }
    #[cfg(target_os = "linux")]
    {
        run_systemctl(&["stop", SYSTEMD_UNIT])
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        anyhow::bail!("daemon controls are not supported on this platform")
    }
}

/// Restart the probe daemon. Falls back to stop+start when the agent
/// isn't currently loaded (so `kickstart -k` would fail).
pub fn restart() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let target = launchd_target();
        if run_launchctl(&["kickstart", "-k", &target]).is_ok() {
            return Ok(());
        }
        let _ = stop();
        start()
    }
    #[cfg(target_os = "linux")]
    {
        run_systemctl(&["restart", SYSTEMD_UNIT])
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        anyhow::bail!("daemon controls are not supported on this platform")
    }
}

#[cfg(target_os = "macos")]
fn launchd_domain() -> String {
    let uid = unsafe { libc::getuid() };
    format!("gui/{}", uid)
}

#[cfg(target_os = "macos")]
fn launchd_target() -> String {
    format!("{}/{}", launchd_domain(), LAUNCHD_LABEL)
}

#[cfg(target_os = "macos")]
fn plist_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let p = std::path::PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"));
    p.exists().then_some(p)
}

#[cfg(target_os = "macos")]
fn run_launchctl(args: &[&str]) -> anyhow::Result<()> {
    run("launchctl", args)
}

#[cfg(target_os = "linux")]
fn run_systemctl(args: &[&str]) -> anyhow::Result<()> {
    // Prefer the user-scope unit (matches the home-manager / desktop
    // install). System-scope `systemctl start` would prompt for a polkit
    // password from a tray app, which isn't workable.
    let mut full = vec!["--user"];
    full.extend_from_slice(args);
    run("systemctl", &full)
}

fn run(cmd: &str, args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("{} {:?} exited with {}", cmd, args, status)
    }
}
