# starla-network

Low-level network primitives for Starla measurements.

## Features

- **Raw Sockets**: ICMP socket creation and management
- **ICMP Utilities**: Packet construction and parsing
- **Capability Management**: Linux capabilities for unprivileged operation
- **Address Resolution**: Source address selection for destinations

## Usage

```rust
use starla_network::{new_icmpv4_socket, IcmpPacket, get_source_addr_for_dest};

// Create raw ICMP socket
let socket = new_icmpv4_socket()?;

// Get appropriate source address for a destination
let src = get_source_addr_for_dest("8.8.8.8".parse()?)?;

// Build and send ICMP echo request
let packet = IcmpPacket::echo_request(identifier, sequence, payload);
socket.send_to(&packet.to_bytes(), dest)?;
```

## Capabilities

On Linux, raw sockets require either:
- `CAP_NET_RAW` capability, or
- Running as root

The `capabilities` module helps manage these requirements.

## Modules

- `raw_socket`: Socket creation and address utilities
- `icmp`: ICMP packet types and parsing
- `capabilities`: Linux capability management
