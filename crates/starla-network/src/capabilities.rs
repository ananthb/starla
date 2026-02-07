//! Linux capabilities management
//!
//! Provides utilities for checking and manipulating process capabilities.

use std::io;

#[cfg(target_os = "linux")]
use tracing::{debug, info};

#[cfg(target_os = "linux")]
use caps::{CapSet, Capability, CapsHashSet};

/// Check if the process has the CAP_NET_RAW capability
pub fn has_net_raw() -> io::Result<bool> {
    #[cfg(target_os = "linux")]
    {
        has_capability(Capability::CAP_NET_RAW)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // On non-Linux (e.g. macOS), we can't check capabilities.
        // We assume true, and let the socket creation fail if permission denied.
        Ok(true)
    }
}

/// Check if the process has a specific capability
#[cfg(target_os = "linux")]
pub fn has_capability(cap: Capability) -> io::Result<bool> {
    let effective =
        caps::read(None, CapSet::Effective).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(effective.contains(&cap))
}

/// Drop all capabilities except those needed for operation (CAP_NET_RAW)
pub fn restrict_capabilities() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        debug!("Restricting capabilities...");

        // We need CAP_NET_RAW for ICMP
        let mut required = CapsHashSet::new();
        required.insert(Capability::CAP_NET_RAW);

        // Check if we currently have them
        let current =
            caps::read(None, CapSet::Effective).map_err(|e| io::Error::other(e.to_string()))?;

        if !current.contains(&Capability::CAP_NET_RAW) {
            debug!("Process does not have CAP_NET_RAW, cannot retain it.");
            // If we don't have it, we can't set it.
            return Ok(());
        }

        // Clear everything else from effective and permitted
        caps::set(None, CapSet::Effective, &required)
            .map_err(|e| io::Error::other(e.to_string()))?;
        caps::set(None, CapSet::Permitted, &required)
            .map_err(|e| io::Error::other(e.to_string()))?;

        info!("Capabilities restricted to: {:?}", required);
    }

    Ok(())
}

/// Drop all capabilities completely
pub fn drop_all_capabilities() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        info!("Dropping all capabilities");
        caps::clear(None, CapSet::Effective).map_err(|e| io::Error::other(e.to_string()))?;
        caps::clear(None, CapSet::Permitted).map_err(|e| io::Error::other(e.to_string()))?;
    }

    Ok(())
}
