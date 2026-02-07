//! Configuration management

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Main probe configuration
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProbeConfig {
    #[serde(default)]
    pub probe: ProbeSettings,

    #[serde(default)]
    pub network: NetworkSettings,

    #[serde(default)]
    pub controller: ControllerSettings,

    #[serde(default)]
    pub storage: StorageSettings,

    #[serde(default)]
    pub metrics: MetricsSettings,

    #[serde(default)]
    pub logging: LoggingSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeSettings {
    /// Firmware version
    #[serde(default = "default_firmware_version")]
    pub firmware_version: u32,

    /// Log level
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

impl Default for ProbeSettings {
    fn default() -> Self {
        Self {
            firmware_version: default_firmware_version(),
            log_level: default_log_level(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkSettings {
    #[serde(default = "default_telnet_port")]
    pub telnet_port: u16,

    #[serde(default = "default_http_post_port")]
    pub http_post_port: u16,

    #[serde(default)]
    pub rxtxrpt: bool,
}

impl Default for NetworkSettings {
    fn default() -> Self {
        Self {
            telnet_port: default_telnet_port(),
            http_post_port: default_http_post_port(),
            rxtxrpt: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerSettings {
    #[serde(default = "default_registration_servers")]
    pub registration_servers: Vec<String>,

    #[serde(default = "default_ssh_timeout")]
    pub ssh_timeout: u64,

    #[serde(default = "default_keepalive_interval")]
    pub keepalive_interval: u64,
}

impl Default for ControllerSettings {
    fn default() -> Self {
        Self {
            registration_servers: default_registration_servers(),
            ssh_timeout: default_ssh_timeout(),
            keepalive_interval: default_keepalive_interval(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageSettings {
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    #[serde(default = "default_max_queue_size_mb")]
    pub max_queue_size_mb: u64,

    #[serde(default = "default_retention_days")]
    pub retention_days: u32,

    #[serde(default = "default_max_database_size_mb")]
    pub max_database_size_mb: u64,

    #[serde(default = "default_cleanup_interval_hours")]
    pub cleanup_interval_hours: u64,
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            max_queue_size_mb: default_max_queue_size_mb(),
            retention_days: default_retention_days(),
            max_database_size_mb: default_max_database_size_mb(),
            cleanup_interval_hours: default_cleanup_interval_hours(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSettings {
    #[serde(default = "default_metrics_enabled")]
    pub enabled: bool,

    #[serde(default = "default_metrics_listen_addr")]
    pub listen_addr: String,
}

impl Default for MetricsSettings {
    fn default() -> Self {
        Self {
            enabled: default_metrics_enabled(),
            listen_addr: default_metrics_listen_addr(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingSettings {
    #[serde(default = "default_log_format")]
    pub format: String,

    #[serde(default = "default_log_output")]
    pub output: String,

    #[serde(default = "default_max_file_size_mb")]
    pub max_file_size_mb: u64,

    #[serde(default = "default_max_files")]
    pub max_files: u32,
}

impl Default for LoggingSettings {
    fn default() -> Self {
        Self {
            format: default_log_format(),
            output: default_log_output(),
            max_file_size_mb: default_max_file_size_mb(),
            max_files: default_max_files(),
        }
    }
}

// Default value functions
fn default_firmware_version() -> u32 {
    crate::FIRMWARE_VERSION
}
fn default_log_level() -> String {
    "info".to_string()
}
fn default_telnet_port() -> u16 {
    2023
}
fn default_http_post_port() -> u16 {
    8080
}
fn default_registration_servers() -> Vec<String> {
    vec![
        // Primary registration server (hostname, IPv4, IPv6)
        "reg03.atlas.ripe.net:443".to_string(),
        "193.0.19.246:443".to_string(),
        "[2001:67c:2e8:11::c100:13f6]:443".to_string(),
        // Secondary registration server (hostname, IPv4, IPv6)
        "reg04.atlas.ripe.net:443".to_string(),
        "193.0.19.247:443".to_string(),
        "[2001:67c:2e8:11::c100:13f7]:443".to_string(),
    ]
}
fn default_ssh_timeout() -> u64 {
    30
}
fn default_keepalive_interval() -> u64 {
    60
}
fn default_data_dir() -> PathBuf {
    crate::paths::state_dir()
}
fn default_max_queue_size_mb() -> u64 {
    100
}
fn default_retention_days() -> u32 {
    30
}
fn default_max_database_size_mb() -> u64 {
    1
}
fn default_cleanup_interval_hours() -> u64 {
    24
}
fn default_metrics_enabled() -> bool {
    true
}
fn default_metrics_listen_addr() -> String {
    "127.0.0.1:9090".to_string()
}
fn default_log_format() -> String {
    "json".to_string()
}
fn default_log_output() -> String {
    "stdout".to_string()
}
fn default_max_file_size_mb() -> u64 {
    10
}
fn default_max_files() -> u32 {
    5
}

impl ProbeConfig {
    /// Load configuration from TOML file
    pub fn from_file(path: &std::path::Path) -> Result<Self, crate::Error> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| crate::Error::Config(format!("Failed to read config: {}", e)))?;

        let config: ProbeConfig = toml::from_str(&content)
            .map_err(|e| crate::Error::Config(format!("Failed to parse config: {}", e)))?;

        Ok(config)
    }

    /// Save configuration to TOML file
    pub fn to_file(&self, path: &std::path::Path) -> Result<(), crate::Error> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| crate::Error::Config(format!("Failed to serialize config: {}", e)))?;

        std::fs::write(path, content)
            .map_err(|e| crate::Error::Config(format!("Failed to write config: {}", e)))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ProbeConfig::default();
        assert_eq!(config.probe.firmware_version, 6000);
        assert_eq!(config.network.telnet_port, 2023);
        assert_eq!(config.network.http_post_port, 8080);
    }

    #[test]
    fn test_config_serde() {
        let config = ProbeConfig::default();
        let toml_str = toml::to_string(&config).unwrap();
        let parsed: ProbeConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.probe.firmware_version, 6000);
    }
}
