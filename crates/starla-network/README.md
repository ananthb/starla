# starla-network

Low-level network primitives for Starla measurements.

## Features

- **ICMP sockets**: raw sockets on Unix, the IP Helper API on Windows
- **ICMP utilities**: packet construction and parsing (`pnet_packet`)
- **Capability management**: Linux capabilities for unprivileged operation
- **Address selection**: the source address for a destination

## Usage

```rust
use starla_network::{build_icmpv4_echo_request, get_source_addr_for_dest};

let dest = "8.8.8.8".parse()?;
let src = get_source_addr_for_dest(dest)?;

let mut buffer = [0u8; 64];
let len = build_icmpv4_echo_request(&mut buffer, identifier, sequence, payload)?;
// send buffer[..len] over a socket from new_icmpv4_socket() (Unix)
```

`build_icmpv4_echo_request` / `build_icmpv6_echo_request` fill an echo
request (returning its length); `parse_icmpv4_packet` /
`parse_icmpv6_packet` read a reply or an error.

## Capabilities

On Linux, raw sockets require either:
- the `CAP_NET_RAW` capability, or
- running as root

The `capabilities` module helps manage these requirements.

## Modules

- `icmp`: ICMP packet types and parsing
- `raw_socket` (Unix): raw socket creation, ancillary data, address utilities
- `windows_icmp` (Windows): the IP Helper API and raw ICMPv6 sockets
- `wsacmsg` (Windows): `WSARecvMsg` ancillary data
- `capabilities` (Linux): capability management
