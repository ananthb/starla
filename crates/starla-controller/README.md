# starla-controller

Handles communication with the RIPE Atlas controller infrastructure.

## Features

- **SSH Tunnel Management**: Secure connection to RIPE Atlas controllers using `russh`
- **Registration Protocol**: INIT/KEEP command handling for probe registration
- **Telnet Interface**: Receives measurement commands from the controller
- **Reverse Port Forwarding**: Allows controller to send commands back to the probe

## Components

### SSH Connection

Manages the SSH tunnel to RIPE Atlas registration and controller servers:

```rust
use starla_controller::{SshConnection, SshConfig, generate_key};

// Generate or load SSH key
let key = generate_key()?;

// Connect to registration servers
let ssh = SshConnection::connect_to_servers(
    &["reg03.atlas.ripe.net:443", "reg04.atlas.ripe.net:443"],
    &key,
    SshConfig::default(),
).await?;
```

### Telnet Server

Receives and parses measurement commands:

```rust
use starla_controller::{TelnetServer, TelnetCommand};

let server = TelnetServer::new(2023, probe_id);
// Commands are parsed into TelnetCommand variants:
// - Ping, Traceroute, Dns, Http, Tls, Ntp
// - Stop, Status
```

## Protocol

The crate implements the RIPE Atlas probe protocol:
1. Connect to registration server via SSH
2. Send INIT with probe info, receive controller assignment
3. Connect to assigned controller
4. Establish reverse tunnel for telnet commands
5. Maintain connection with KEEP messages
