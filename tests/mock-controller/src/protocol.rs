//! RIPE Atlas protocol types

use serde::{Deserialize, Serialize};

/// Response to INIT command
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitResponse {
    pub controller_host: String,
    pub controller_port: u16,
    pub probe_id: u32,
    pub firmware_version: u32,
}

impl InitResponse {
    pub fn to_protocol_string(&self) -> String {
        format!(
            "CONTROLLER {} {} {}\n",
            self.controller_host, self.controller_port, self.probe_id
        )
    }
}

/// Response to KEEP command
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeepResponse {
    pub status: String,
}

impl KeepResponse {
    pub fn ok() -> Self {
        Self {
            status: "OK".to_string(),
        }
    }

    pub fn to_protocol_string(&self) -> String {
        format!("{}\n", self.status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_response() {
        let response = InitResponse {
            controller_host: "127.0.0.1".to_string(),
            controller_port: 2222,
            probe_id: 99999,
            firmware_version: 6000,
        };

        let protocol = response.to_protocol_string();
        assert!(protocol.contains("CONTROLLER"));
        assert!(protocol.contains("127.0.0.1"));
    }

    #[test]
    fn test_keep_response() {
        let response = KeepResponse::ok();
        assert_eq!(response.to_protocol_string(), "OK\n");
    }
}
