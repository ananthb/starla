# starla-common

Common types and utilities shared across all Starla components.

## Features

- **Core Types**: `ProbeId`, `MeasurementId`, `MeasurementResult`, and other shared types
- **Configuration**: TOML-based configuration with sensible defaults
- **Path Resolution**: XDG Base Directory and systemd integration for config/state paths
- **Logging**: Flexible logging infrastructure with stdout, file, and JSON output
- **Error Types**: Unified error handling across crates

## Usage

```rust
use starla_common::{ProbeId, MeasurementId, config_dir, state_dir};
use starla_common::logging::{LogConfig, init_logging};

// Initialize logging (stdout with info level by default)
init_logging(LogConfig::default()).unwrap();

// Or with custom configuration
let log_config = LogConfig {
    log_dir: Some("/var/log/starla".into()),
    level: "debug".to_string(),
    json_format: false,
};
init_logging(log_config).unwrap();

// Access configuration and state directories
let config = config_dir();  // ~/.config/starla or $XDG_CONFIG_HOME/starla
let state = state_dir();    // ~/.local/state/starla or $XDG_STATE_HOME/starla

// Use core types
let probe_id = ProbeId(1234);
let msm_id = MeasurementId(5678);
```

## Path Resolution

Configuration directory priority:
1. `$CONFIGURATION_DIRECTORY` (systemd)
2. `$XDG_CONFIG_HOME/starla`
3. `~/.config/starla`

State directory priority:
1. `$STATE_DIRECTORY` (systemd)
2. `$XDG_STATE_HOME/starla`
3. `~/.local/state/starla`

## Cargo Features

- `json` - Enable JSON structured logging format (useful for log aggregation systems)
