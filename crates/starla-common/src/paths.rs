//! Path resolution for Starla
//!
//! Directory resolution priority:
//!
//! **Config directory** (for config.toml):
//! 1. `$CONFIGURATION_DIRECTORY` (systemd)
//! 2. Container: `/config`
//! 3. `$XDG_CONFIG_HOME/starla`
//! 4. Root: `/etc/starla`, non-root: `~/.config/starla`
//!
//! **State directory** (for keys, probe_id, known_hosts):
//! 1. CLI `--state-dir` (via override)
//! 2. `$STATE_DIRECTORY` (systemd)
//! 3. Container: `/state`
//! 4. `$XDG_STATE_HOME/starla`
//! 5. Root: `/var/lib/starla`, non-root: `~/.local/state/starla`
//!
//! **Runtime directory** (for ephemeral databases, caches):
//! 1. `$RUNTIME_DIRECTORY` (systemd)
//! 2. Container: `/run/starla`
//! 3. `$XDG_RUNTIME_DIR/starla`
//! 4. Root: `/run/starla`, non-root: `/tmp/starla-<uid>`

use std::path::PathBuf;
use std::sync::OnceLock;

/// Application name used in subdirectories
const APP_NAME: &str = "starla";

/// Detect if running inside a container (Docker, Podman, etc.)
///
/// Checks for:
/// - `container` env var (set by podman, systemd-nspawn)
/// - `/.dockerenv` file (Docker)
/// - `/run/.containerenv` file (Podman)
fn is_container() -> bool {
    std::env::var("container").is_ok()
        || std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
}

/// CLI override for state directory (set once at startup via `set_state_dir`)
static STATE_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// CLI override for runtime directory (set once at startup via
/// `set_runtime_dir`)
static RUNTIME_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Set the state directory override (from `--state-dir` CLI arg).
/// Must be called before any `state_dir()` calls. Subsequent calls are ignored.
pub fn set_state_dir(path: PathBuf) {
    let _ = STATE_DIR_OVERRIDE.set(path);
}

/// Set the runtime directory override (from `--runtime-dir` CLI arg).
/// Must be called before any `runtime_dir()` calls. Subsequent calls are
/// ignored.
pub fn set_runtime_dir(path: PathBuf) {
    let _ = RUNTIME_DIR_OVERRIDE.set(path);
}

/// Get the configuration directory path
///
/// Priority:
/// 1. `$CONFIGURATION_DIRECTORY` (systemd)
/// 2. Container: `/config`
/// 3. `$XDG_CONFIG_HOME/starla`
/// 4. Root: `/etc/starla`, non-root: `~/.config/starla`
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CONFIGURATION_DIRECTORY") {
        return PathBuf::from(dir);
    }

    if is_container() {
        return PathBuf::from("/config");
    }

    if let Ok(xdg_config) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg_config).join(APP_NAME);
    }

    if is_root() {
        return PathBuf::from("/etc").join(APP_NAME);
    }

    if let Some(home) = home_dir() {
        return home.join(".config").join(APP_NAME);
    }

    PathBuf::from("/etc").join(APP_NAME)
}

/// Get the state directory path (for databases, keys, etc.)
///
/// Priority:
/// 1. CLI override (set via `set_state_dir`)
/// 2. `$STATE_DIRECTORY` (systemd)
/// 3. Container: `/state`
/// 4. `$XDG_STATE_HOME/starla`
/// 5. Root: `/var/lib/starla`, non-root: `~/.local/state/starla`
pub fn state_dir() -> PathBuf {
    if let Some(override_dir) = STATE_DIR_OVERRIDE.get() {
        return override_dir.clone();
    }

    if let Ok(dir) = std::env::var("STATE_DIRECTORY") {
        return PathBuf::from(dir);
    }

    if is_container() {
        return PathBuf::from("/state");
    }

    if let Ok(xdg_state) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(xdg_state).join(APP_NAME);
    }

    if is_root() {
        return PathBuf::from("/var/lib").join(APP_NAME);
    }

    if let Some(home) = home_dir() {
        return home.join(".local").join("state").join(APP_NAME);
    }

    PathBuf::from("/var/lib").join(APP_NAME)
}

/// Get the runtime directory path (for ephemeral data: databases, caches)
///
/// Priority:
/// 1. CLI override (set via `set_runtime_dir`)
/// 2. `$RUNTIME_DIRECTORY` (systemd)
/// 3. Container: `/run/starla`
/// 4. `$XDG_RUNTIME_DIR/starla`
/// 5. Root: `/run/starla`, non-root: `/tmp/starla-<uid>`
pub fn runtime_dir() -> PathBuf {
    if let Some(override_dir) = RUNTIME_DIR_OVERRIDE.get() {
        return override_dir.clone();
    }

    if let Ok(dir) = std::env::var("RUNTIME_DIRECTORY") {
        return PathBuf::from(dir);
    }

    if is_container() {
        return PathBuf::from("/run").join(APP_NAME);
    }

    if let Ok(xdg_runtime) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(xdg_runtime).join(APP_NAME);
    }

    if is_root() {
        return PathBuf::from("/run").join(APP_NAME);
    }

    // Per-user temp directory
    let uid = {
        #[cfg(unix)]
        {
            unsafe { libc::getuid() }
        }
        #[cfg(not(unix))]
        {
            0u32
        }
    };
    std::env::temp_dir().join(format!("{}-{}", APP_NAME, uid))
}

/// Get the default config file path
pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

/// Get the default probe key path
pub fn probe_key_path() -> PathBuf {
    state_dir().join("probe_key")
}

/// Get the default probe public key path
pub fn probe_pubkey_path() -> PathBuf {
    state_dir().join("probe_key.pub")
}

/// Get the known SSH host keys path
pub fn known_hosts_path() -> PathBuf {
    state_dir().join("known_hosts")
}

/// Get the probe ID file path
pub fn probe_id_path() -> PathBuf {
    state_dir().join("probe_id")
}

/// Read the probe ID from the state directory
///
/// Returns None if the file doesn't exist or can't be parsed
pub fn read_probe_id() -> Option<u32> {
    let path = probe_id_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => content.trim().parse().ok(),
        Err(_) => None,
    }
}

/// Write the probe ID to the state directory
///
/// Creates the state directory if it doesn't exist
pub fn write_probe_id(probe_id: u32) -> std::io::Result<()> {
    let dir = state_dir();
    ensure_dir(&dir)?;
    let path = probe_id_path();
    std::fs::write(&path, probe_id.to_string())
}

/// Check if the current process is running as root (UID 0)
#[cfg(unix)]
fn is_root() -> bool {
    // Safety: getuid() is a simple syscall with no safety concerns
    unsafe { libc::getuid() == 0 }
}

#[cfg(not(unix))]
fn is_root() -> bool {
    false
}

/// Get the user's home directory
fn home_dir() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("HOME") {
        return Some(PathBuf::from(home));
    }

    #[cfg(windows)]
    if let Ok(home) = std::env::var("USERPROFILE") {
        return Some(PathBuf::from(home));
    }

    None
}

/// Ensure a directory exists, creating it if necessary
pub fn ensure_dir(path: &PathBuf) -> std::io::Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)?;
    }
    Ok(())
}

/// Ensure the config directory exists
pub fn ensure_config_dir() -> std::io::Result<PathBuf> {
    let dir = config_dir();
    ensure_dir(&dir)?;
    Ok(dir)
}

/// Ensure the state directory exists
pub fn ensure_state_dir() -> std::io::Result<PathBuf> {
    let dir = state_dir();
    ensure_dir(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Mutex to serialize tests that modify environment variables.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_config_dir_with_env() {
        let _guard = ENV_LOCK.lock().unwrap();

        let original = std::env::var("CONFIGURATION_DIRECTORY").ok();

        unsafe { std::env::set_var("CONFIGURATION_DIRECTORY", "/test/config") };
        assert_eq!(config_dir(), PathBuf::from("/test/config"));

        if let Some(val) = original {
            unsafe { std::env::set_var("CONFIGURATION_DIRECTORY", val) };
        } else {
            unsafe { std::env::remove_var("CONFIGURATION_DIRECTORY") };
        }
    }

    #[test]
    fn test_state_dir_with_env() {
        let _guard = ENV_LOCK.lock().unwrap();

        let original = std::env::var("STATE_DIRECTORY").ok();

        unsafe { std::env::set_var("STATE_DIRECTORY", "/test/state") };
        assert_eq!(state_dir(), PathBuf::from("/test/state"));

        if let Some(val) = original {
            unsafe { std::env::set_var("STATE_DIRECTORY", val) };
        } else {
            unsafe { std::env::remove_var("STATE_DIRECTORY") };
        }
    }

    #[test]
    fn test_xdg_config_fallback() {
        let _guard = ENV_LOCK.lock().unwrap();

        let orig_conf_dir = std::env::var("CONFIGURATION_DIRECTORY").ok();
        let orig_xdg = std::env::var("XDG_CONFIG_HOME").ok();

        unsafe { std::env::remove_var("CONFIGURATION_DIRECTORY") };
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/home/test/.config") };

        assert_eq!(config_dir(), PathBuf::from("/home/test/.config/starla"));

        if let Some(val) = orig_conf_dir {
            unsafe { std::env::set_var("CONFIGURATION_DIRECTORY", val) };
        }
        if let Some(val) = orig_xdg {
            unsafe { std::env::set_var("XDG_CONFIG_HOME", val) };
        } else {
            unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        }
    }

    #[test]
    fn test_xdg_state_fallback() {
        let _guard = ENV_LOCK.lock().unwrap();

        let orig_state_dir = std::env::var("STATE_DIRECTORY").ok();
        let orig_xdg = std::env::var("XDG_STATE_HOME").ok();

        unsafe { std::env::remove_var("STATE_DIRECTORY") };
        unsafe { std::env::set_var("XDG_STATE_HOME", "/home/test/.local/state") };

        assert_eq!(state_dir(), PathBuf::from("/home/test/.local/state/starla"));

        if let Some(val) = orig_state_dir {
            unsafe { std::env::set_var("STATE_DIRECTORY", val) };
        }
        if let Some(val) = orig_xdg {
            unsafe { std::env::set_var("XDG_STATE_HOME", val) };
        } else {
            unsafe { std::env::remove_var("XDG_STATE_HOME") };
        }
    }

    #[test]
    fn test_default_file_paths() {
        let config = config_file();
        assert!(config.to_string_lossy().contains("config.toml"));

        let key = probe_key_path();
        assert!(key.to_string_lossy().contains("probe_key"));

        let pid = probe_id_path();
        assert!(pid.to_string_lossy().contains("probe_id"));

        let kh = known_hosts_path();
        assert!(kh.to_string_lossy().contains("known_hosts"));
    }

    #[test]
    fn test_probe_id_read_write() {
        let temp_dir =
            std::env::temp_dir().join(format!("starla-test-probe-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let probe_id_file = temp_dir.join("probe_id");
        assert!(!probe_id_file.exists());

        std::fs::write(&probe_id_file, "1014036").unwrap();

        let content = std::fs::read_to_string(&probe_id_file).unwrap();
        let parsed: Option<u32> = content.trim().parse().ok();
        assert_eq!(parsed, Some(1014036));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
