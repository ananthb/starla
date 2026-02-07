//! Path resolution for Starla
//!
//! This module handles locating configuration and state directories following:
//! 1. Systemd service directories (CONFIGURATION_DIRECTORY, STATE_DIRECTORY)
//! 2. XDG Base Directory Specification (XDG_CONFIG_HOME, XDG_STATE_HOME)
//! 3. Default fallbacks
//!
//! ## Directory Layout
//!
//! Configuration directory (for config.toml):
//! - If `CONFIGURATION_DIRECTORY` is set: use it directly
//! - Else if `XDG_CONFIG_HOME` is set: `$XDG_CONFIG_HOME/starla/`
//! - Else: `~/.config/starla/`
//!
//! State directory (for databases, probe key):
//! - If `STATE_DIRECTORY` is set: use it directly
//! - Else if `XDG_STATE_HOME` is set: `$XDG_STATE_HOME/starla/`
//! - Else: `~/.local/state/starla/`

use std::path::PathBuf;

/// Application name used in XDG subdirectories
const APP_NAME: &str = "starla";

/// Get the configuration directory path
///
/// Priority:
/// 1. `CONFIGURATION_DIRECTORY` environment variable (systemd)
/// 2. `XDG_CONFIG_HOME/starla` (XDG spec)
/// 3. `~/.config/starla` (XDG default)
pub fn config_dir() -> PathBuf {
    // Check systemd CONFIGURATION_DIRECTORY first
    if let Ok(dir) = std::env::var("CONFIGURATION_DIRECTORY") {
        return PathBuf::from(dir);
    }

    // Check XDG_CONFIG_HOME
    if let Ok(xdg_config) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg_config).join(APP_NAME);
    }

    // Default to ~/.config/starla
    if let Some(home) = home_dir() {
        return home.join(".config").join(APP_NAME);
    }

    // Ultimate fallback
    PathBuf::from("/etc").join(APP_NAME)
}

/// Get the state directory path (for databases, keys, etc.)
///
/// Priority:
/// 1. `STATE_DIRECTORY` environment variable (systemd)
/// 2. `XDG_STATE_HOME/starla` (XDG spec)
/// 3. `~/.local/state/starla` (XDG default)
pub fn state_dir() -> PathBuf {
    // Check systemd STATE_DIRECTORY first
    if let Ok(dir) = std::env::var("STATE_DIRECTORY") {
        return PathBuf::from(dir);
    }

    // Check XDG_STATE_HOME
    if let Ok(xdg_state) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(xdg_state).join(APP_NAME);
    }

    // Default to ~/.local/state/starla
    if let Some(home) = home_dir() {
        return home.join(".local").join("state").join(APP_NAME);
    }

    // Ultimate fallback
    PathBuf::from("/var/lib").join(APP_NAME)
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

/// Get the default database path
pub fn database_path() -> PathBuf {
    state_dir().join("measurements.db")
}

/// Get the default results queue database path
pub fn results_queue_path() -> PathBuf {
    state_dir().join("results_queue.db")
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

/// Get the user's home directory
fn home_dir() -> Option<PathBuf> {
    // Try HOME environment variable (works on Unix and most systems)
    if let Ok(home) = std::env::var("HOME") {
        return Some(PathBuf::from(home));
    }

    // On Windows, try USERPROFILE
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

    #[test]
    fn test_config_dir_with_env() {
        // Save original value
        let original = std::env::var("CONFIGURATION_DIRECTORY").ok();

        // Test with CONFIGURATION_DIRECTORY set
        std::env::set_var("CONFIGURATION_DIRECTORY", "/test/config");
        assert_eq!(config_dir(), PathBuf::from("/test/config"));

        // Restore original
        if let Some(val) = original {
            std::env::set_var("CONFIGURATION_DIRECTORY", val);
        } else {
            std::env::remove_var("CONFIGURATION_DIRECTORY");
        }
    }

    #[test]
    fn test_state_dir_with_env() {
        // Save original value
        let original = std::env::var("STATE_DIRECTORY").ok();

        // Test with STATE_DIRECTORY set
        std::env::set_var("STATE_DIRECTORY", "/test/state");
        assert_eq!(state_dir(), PathBuf::from("/test/state"));

        // Restore original
        if let Some(val) = original {
            std::env::set_var("STATE_DIRECTORY", val);
        } else {
            std::env::remove_var("STATE_DIRECTORY");
        }
    }

    #[test]
    fn test_xdg_config_fallback() {
        // Save original values
        let orig_conf_dir = std::env::var("CONFIGURATION_DIRECTORY").ok();
        let orig_xdg = std::env::var("XDG_CONFIG_HOME").ok();

        // Clear CONFIGURATION_DIRECTORY, set XDG_CONFIG_HOME
        std::env::remove_var("CONFIGURATION_DIRECTORY");
        std::env::set_var("XDG_CONFIG_HOME", "/home/test/.config");

        assert_eq!(config_dir(), PathBuf::from("/home/test/.config/starla"));

        // Restore
        if let Some(val) = orig_conf_dir {
            std::env::set_var("CONFIGURATION_DIRECTORY", val);
        }
        if let Some(val) = orig_xdg {
            std::env::set_var("XDG_CONFIG_HOME", val);
        } else {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
    }

    #[test]
    fn test_xdg_state_fallback() {
        // Save original values
        let orig_state_dir = std::env::var("STATE_DIRECTORY").ok();
        let orig_xdg = std::env::var("XDG_STATE_HOME").ok();

        // Clear STATE_DIRECTORY, set XDG_STATE_HOME
        std::env::remove_var("STATE_DIRECTORY");
        std::env::set_var("XDG_STATE_HOME", "/home/test/.local/state");

        assert_eq!(state_dir(), PathBuf::from("/home/test/.local/state/starla"));

        // Restore
        if let Some(val) = orig_state_dir {
            std::env::set_var("STATE_DIRECTORY", val);
        }
        if let Some(val) = orig_xdg {
            std::env::set_var("XDG_STATE_HOME", val);
        } else {
            std::env::remove_var("XDG_STATE_HOME");
        }
    }

    #[test]
    fn test_default_file_paths() {
        // Just verify these don't panic and return sensible paths
        let config = config_file();
        assert!(config.to_string_lossy().contains("config.toml"));

        let key = probe_key_path();
        assert!(key.to_string_lossy().contains("probe_key"));

        let db = database_path();
        assert!(db.to_string_lossy().contains("measurements.db"));

        let probe_id = probe_id_path();
        assert!(probe_id.to_string_lossy().contains("probe_id"));
    }

    #[test]
    fn test_probe_id_read_write() {
        // Use a temp directory for this test
        let temp_dir = std::env::temp_dir().join("starla-test-probe-id");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Save original STATE_DIRECTORY
        let orig_state_dir = std::env::var("STATE_DIRECTORY").ok();
        std::env::set_var("STATE_DIRECTORY", &temp_dir);

        // Initially should return None
        assert_eq!(read_probe_id(), None);

        // Write a probe ID
        write_probe_id(1014036).unwrap();

        // Should now read it back
        assert_eq!(read_probe_id(), Some(1014036));

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);

        // Restore original
        if let Some(val) = orig_state_dir {
            std::env::set_var("STATE_DIRECTORY", val);
        } else {
            std::env::remove_var("STATE_DIRECTORY");
        }
    }
}
